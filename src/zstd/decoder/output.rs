//! Where the encoder and the decoder write to.

use crate::error::{Error, Result};

/// Longer copies can crawl on Zen 3 and 4: https://sourceware.org/bugzilla/show_bug.cgi?id=30994
const PIECE_LEN: usize = 2048;
/// They do if the target is a few bytes ahead of the source modulo this.
const ALIAS_PERIOD: usize = 4096;
/// No system has smaller ones.
const PAGE_LEN: usize = 4096;

/// A slice, or the spare capacity of a `Vec`.
pub struct Output<'a> {
    target: Target<'a>,
    start: usize,
    position: usize,
    end: usize,
    /// Whether [`reserve`](Self::reserve) asks for memory.
    grows: bool,
}

enum Target<'a> {
    Slice(&'a mut [u8]),
    /// Longer than `position` by what was lent, until the `Output` drops.
    Vec(&'a mut Vec<u8>),
    /// Stays as long as it got.
    Ring(&'a mut Vec<u8>),
}

/// Those that start before `end`. It takes longer to wait for new ones within `memset()` or `memcpy()`.
#[inline]
fn touch_pages(vec: &mut Vec<u8>, end: usize) {
    let len = end.saturating_sub(vec.len());
    let spare = vec.spare_capacity_mut();
    let first = spare.as_ptr().addr().wrapping_neg() % PAGE_LEN;
    let pages = spare.get_mut(first..len).unwrap_or_default();
    for byte in pages.iter_mut().step_by(PAGE_LEN) {
        byte.write(0);
    }
}

fn clear_up_to(vec: &mut Vec<u8>, len: usize) {
    touch_pages(vec, len);
    // New pages are quicker to clear without `rep stosb`, too.
    while vec.len() < len {
        vec.resize(len.min(vec.len() + PIECE_LEN), 0);
    }
}

impl Target<'_> {
    #[inline]
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Slice(slice) => slice,
            Self::Vec(vec) | Self::Ring(vec) => vec,
        }
    }

    #[inline]
    fn capacity(&self) -> usize {
        match self {
            Self::Slice(slice) => slice.len(),
            Self::Vec(vec) | Self::Ring(vec) => vec.capacity(),
        }
    }

    /// At least `len`, for which there is capacity.
    #[inline]
    fn bytes_mut(&mut self, len: usize) -> &mut [u8] {
        match self {
            Self::Slice(slice) => slice,
            Self::Vec(vec) | Self::Ring(vec) => {
                if vec.len() < len {
                    clear_up_to(vec, len);
                }
                vec
            }
        }
    }

    /// Cut off at `start`, if `len` bytes from there have to be appended.
    #[inline]
    fn to_extend(&mut self, start: usize, len: usize) -> Option<&mut Vec<u8>> {
        match self {
            Self::Vec(vec) | Self::Ring(vec) if (start..start + len).contains(&vec.len()) => {
                touch_pages(vec, start + len);
                vec.truncate(start);
                Some(vec)
            }
            _ => None,
        }
    }

    #[inline]
    fn write(&mut self, start: usize, bytes: &[u8]) {
        let end = start + bytes.len();
        debug_assert!(start <= self.bytes().len() && end <= self.capacity());
        if let Some(vec) = self.to_extend(start, bytes.len()) {
            vec.extend_from_slice(bytes);
        } else if let Some(to) = self.bytes_mut(0).get_mut(start..end) {
            to.copy_from_slice(bytes);
        }
    }

    #[inline(never)]
    fn write_many(&mut self, start: usize, bytes: &[u8]) {
        let address = self.bytes().as_ptr().addr().wrapping_add(start);
        let lead = address.wrapping_sub(bytes.as_ptr().addr()) % ALIAS_PERIOD;
        if !(1..32).contains(&lead) {
            return self.write(start, bytes);
        }
        for (index, piece) in bytes.chunks(PIECE_LEN).enumerate() {
            self.write(start + index * PIECE_LEN, piece);
        }
    }
}

impl<'a> Output<'a> {
    pub fn slice(slice: &'a mut [u8]) -> Self {
        Self::resume_slice(slice, 0)
    }

    /// Appends at most `max` bytes.
    pub fn spare_capacity(vec: &'a mut Vec<u8>, max: usize) -> Self {
        Self {
            start: vec.len(),
            position: vec.len(),
            end: vec.len() + max.min(vec.capacity() - vec.len()),
            grows: false,
            target: Target::Vec(vec),
        }
    }

    /// The same, with room for `max` bytes. Only those that were [`reserve`](Self::reserve)d are written.
    pub(crate) fn growing(vec: &'a mut Vec<u8>, max: usize) -> Self {
        Self {
            start: vec.len(),
            position: vec.len(),
            end: vec.len().saturating_add(max),
            grows: true,
            target: Target::Vec(vec),
        }
    }

    /// Goes on at `position`.
    pub(crate) fn resume_slice(slice: &'a mut [u8], position: usize) -> Self {
        Self {
            start: 0,
            position: position.min(slice.len()),
            end: slice.len(),
            grows: false,
            target: Target::Slice(slice),
        }
    }

    /// Goes on at `position` of `ring`, which is at least that long. The capacity behind `end` is slack.
    pub fn resume_ring(ring: &'a mut Vec<u8>, position: usize, end: usize) -> Self {
        let end = end.min(ring.capacity());
        Self {
            start: 0,
            position: position.min(end).min(ring.len()),
            end,
            grows: false,
            target: Target::Ring(ring),
        }
    }

    #[inline]
    pub fn written(&self) -> usize {
        self.position - self.start
    }

    #[inline]
    pub fn room(&self) -> usize {
        self.end - self.position
    }

    /// How far [`lend_with_slack`](Self::lend_with_slack) can go.
    #[inline]
    pub(crate) fn room_with_slack(&self) -> usize {
        self.target.capacity() - self.position
    }

    /// The room for which there is memory.
    #[inline]
    pub(crate) fn reserved_room(&self) -> usize {
        self.room().min(self.room_with_slack())
    }

    pub(crate) fn grows(&self) -> bool {
        self.grows
    }

    /// How many bytes it takes along if it has to move in order to grow.
    pub(crate) fn len_to_move(&self) -> usize {
        self.position
    }

    /// With how many bytes are missing for the next `len`.
    #[inline]
    fn to_grow(&mut self, len: usize) -> Option<(&mut Vec<u8>, usize)> {
        match &mut self.target {
            Target::Vec(vec) if self.grows => {
                let missing = (self.position + len).saturating_sub(vec.len());
                Some((vec, missing))
            }
            _ => None,
        }
    }

    /// For the next `len` bytes, or more.
    #[inline]
    pub(crate) fn reserve(&mut self, len: usize) -> Result<()> {
        let result = self
            .to_grow(len)
            .map(|(vec, missing)| vec.try_reserve(missing));
        Ok(result.unwrap_or(Ok(()))?)
    }

    /// For the last `len` bytes.
    pub(crate) fn reserve_exact(&mut self, len: usize) -> Result<()> {
        let result = (self.to_grow(len)).map(|(vec, missing)| vec.try_reserve_exact(missing));
        Ok(result.unwrap_or(Ok(()))?)
    }

    /// Writes as many as there is room for, and returns how many.
    #[inline]
    pub fn append(&mut self, bytes: &[u8]) -> usize {
        let bytes = bytes.get(..self.reserved_room()).unwrap_or(bytes);
        self.write_ahead(0, bytes);
        self.position += bytes.len();
        bytes.len()
    }

    /// Behind `ahead` bytes of what was lent or written like this. There has to be room.
    #[inline]
    pub(crate) fn write_ahead(&mut self, ahead: usize, bytes: &[u8]) {
        let start = self.position + ahead;
        if bytes.len() > PIECE_LEN {
            self.target.write_many(start, bytes);
        } else if !bytes.is_empty() {
            self.target.write(start, bytes);
        }
    }

    /// The same for `len` bytes, each of which is the one `distance` before it.
    #[inline]
    pub(crate) fn repeat_ahead(&mut self, ahead: usize, distance: usize, len: usize) -> Result<()> {
        let mut to = self.position + ahead;
        let from = to.checked_sub(distance).filter(|&from| from < to);
        let (mut from, end) = (from.ok_or(Error::Internal)?, to + len);
        while to < end {
            let len = (to - from).min(end - to).min(PIECE_LEN);
            if let Some(vec) = self.target.to_extend(to, len) {
                vec.extend_from_within(from..from + len);
            } else {
                let parts = self.target.bytes_mut(0).split_at_mut_checked(to);
                let (written, room) = parts.ok_or(Error::Internal)?;
                let piece = written.get(from..from + len).ok_or(Error::Internal)?;
                (room.get_mut(..len).ok_or(Error::Internal)?).copy_from_slice(piece);
            }
            // Until it is as long as a piece, the distance doubles.
            if len < to - from {
                from += len;
            }
            to += len;
        }
        Ok(())
    }

    /// Writes `byte` up to `count` times.
    pub(crate) fn append_repeated(&mut self, byte: u8, count: usize) {
        let end = self.position + count.min(self.reserved_room());
        if let Some(vec) = self.target.to_extend(self.position, end - self.position) {
            vec.resize(end, byte);
        } else if let Some(to) = self.target.bytes_mut(0).get_mut(self.position..end) {
            to.fill(byte);
        }
        self.position = end;
    }

    /// The last `len` bytes that were written.
    #[inline]
    pub(crate) fn last_written(&self, len: usize) -> &[u8] {
        let start = self.position - len.min(self.written());
        (self.target.bytes().get(start..self.position)).unwrap_or_default()
    }

    /// The last `kept` bytes, and `len` more as far as there is memory, or up to `most` if that comes for free.
    #[inline]
    pub(crate) fn lend_with_slack(&mut self, kept: usize, len: usize, most: usize) -> &mut [u8] {
        let start = self.position - kept.min(self.written());
        let (end, most) = (
            self.position + len.min(self.room_with_slack()),
            self.position + most,
        );
        let bytes = self.target.bytes_mut(end);
        let end = bytes.len().min(most);
        bytes.get_mut(start..end).unwrap_or_default()
    }

    /// What was written, and up to `len` bytes of the room behind it. These count once they are [`commit`](Self::commit)ted.
    pub fn lend(&mut self, len: usize) -> &mut [u8] {
        let end = self.position + len.min(self.reserved_room());
        (self.target.bytes_mut(end).get_mut(self.start..end)).unwrap_or_default()
    }

    #[inline]
    pub fn commit(&mut self, len: usize) {
        let most = self.room().min(self.target.bytes().len() - self.position);
        debug_assert!(len <= most);
        self.position += len.min(most);
    }

    /// Forgets all but the first `written` bytes.
    pub fn rewind(&mut self, written: usize) {
        self.position = self.position.min(self.start.saturating_add(written));
    }
}

impl Drop for Output<'_> {
    fn drop(&mut self) {
        if let Target::Vec(vec) = &mut self.target {
            vec.truncate(self.position);
        }
    }
}
