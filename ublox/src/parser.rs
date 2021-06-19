use alloc::vec::Vec;

use crate::{
    error::ParserError,
    ubx_packets::{
        match_packet, ubx_checksum, PacketRef, MAX_PAYLOAD_LEN, SYNC_CHAR_1, SYNC_CHAR_2,
    },
};

pub trait UnderlyingBuffer: core::ops::Index<core::ops::Range<usize>, Output = [u8]> {
    fn clear(&mut self);
    fn len(&self) -> usize;
    fn max_capacity(&self) -> usize;

    /// Returns the number of bytes not copied over due to buffer size constraints
    fn extend_from_slice(&mut self, other: &[u8]) -> usize;

    fn drain(&mut self, count: usize);
    fn find(&self, value: u8) -> Option<usize>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl UnderlyingBuffer for Vec<u8> {
    fn clear(&mut self) {
        self.clear();
    }

    fn len(&self) -> usize {
        self.len()
    }

    fn max_capacity(&self) -> usize {
        usize::MAX
    }

    fn extend_from_slice(&mut self, other: &[u8]) -> usize {
        self.extend_from_slice(other);
        0
    }

    fn drain(&mut self, count: usize) {
        self.drain(0..count);
    }

    fn find(&self, value: u8) -> Option<usize> {
        self.iter().position(|elem| *elem == value)
    }
}

pub struct FixedLinearBuffer<'a> {
    buffer: &'a mut [u8],
    len: usize,
}

impl<'a> FixedLinearBuffer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buffer: buf,
            len: 0,
        }
    }
}

impl<'a> core::ops::Index<core::ops::Range<usize>> for FixedLinearBuffer<'a> {
    type Output = [u8];

    fn index(&self, index: core::ops::Range<usize>) -> &Self::Output {
        if index.end > self.len {
            panic!("Index {} is outside of our length {}", index.end, self.len);
        }
        self.buffer.index(index)
    }
}

impl<'a> UnderlyingBuffer for FixedLinearBuffer<'a> {
    fn clear(&mut self) {
        self.len = 0;
    }

    fn len(&self) -> usize {
        self.len
    }

    fn max_capacity(&self) -> usize {
        self.buffer.len()
    }

    fn extend_from_slice(&mut self, other: &[u8]) -> usize {
        let to_copy = core::cmp::min(other.len(), self.buffer.len() - self.len);
        let uncopyable = other.len() - to_copy;
        for idx in 0..to_copy {
            self.buffer[idx + self.len] = other[idx];
        }
        self.len += to_copy;
        uncopyable
    }

    fn drain(&mut self, count: usize) {
        if count >= self.len {
            self.len = 0;
            return;
        }

        let new_size = self.len - count;
        for idx in 0..new_size {
            self.buffer[idx] = self.buffer[idx + count];
        }
        self.len = new_size;
    }

    fn find(&self, value: u8) -> Option<usize> {
        for i in 0..self.len {
            if self.buffer[i] == value {
                return Some(i);
            }
        }
        None
    }
}

/// Streaming parser for UBX protocol with buffer. The default constructor will build
/// a parser containing a Vec, but you can pass your own underlying buffer by passing it
/// to Parser::new().
///
/// If you pass your own buffer, it should be able to store at _least_ 4 bytes. In practice,
/// you won't be able to do anything useful unless it's at least 36 bytes long (the size
/// of a NavPosLlh packet).
pub struct Parser<T = Vec<u8>>
where
    T: UnderlyingBuffer,
{
    buf: T,
}

impl std::default::Default for Parser<Vec<u8>> {
    fn default() -> Self {
        Self { buf: vec![] }
    }
}

impl<T: UnderlyingBuffer> Parser<T> {
    pub fn new(underlying: T) -> Self {
        Self { buf: underlying }
    }

    pub fn is_buffer_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn buffer_len(&self) -> usize {
        self.buf.len()
    }

    pub fn consume(&mut self, new_data: &[u8]) -> ParserIter<T> {
        let start_idx = match self.buf.find(SYNC_CHAR_1) {
            Some(idx) => Some(idx),
            None => new_data
                .iter()
                .position(|elem| *elem == SYNC_CHAR_1)
                .map(|x| x + self.buf.len()),
        };

        match start_idx {
            Some(mut off) => {
                if off >= self.buf.len() {
                    off -= self.buf.len();
                    self.buf.clear();
                    self.buf.extend_from_slice(&new_data[off..]);
                    off = 0;
                } else {
                    self.buf.extend_from_slice(new_data);
                }
                ParserIter {
                    buf: &mut self.buf,
                    off,
                }
            }
            None => {
                self.buf.clear();
                ParserIter {
                    buf: &mut self.buf,
                    off: 0,
                }
            }
        }
    }
}

/// Iterator over data stored in `Parser` buffer
pub struct ParserIter<'a, T: UnderlyingBuffer> {
    buf: &'a mut T,
    off: usize,
}

impl<'a, T: UnderlyingBuffer> Drop for ParserIter<'a, T> {
    fn drop(&mut self) {
        if self.off <= self.buf.len() {
            self.buf.drain(self.off);
        }
    }
}

impl<'a, T: UnderlyingBuffer> ParserIter<'a, T> {
    /// Analog of `core::iter::Iterator::next`, should be switched to
    /// trait implementation after merge of https://github.com/rust-lang/rust/issues/44265
    pub fn next(&mut self) -> Option<Result<PacketRef, ParserError>> {
        while self.off < self.buf.len() {
            let data = &self.buf[self.off..self.buf.len()];
            let pos = data.iter().position(|x| *x == SYNC_CHAR_1)?;
            let maybe_pack = &data[pos..];

            if maybe_pack.len() <= 1 {
                return None;
            }
            if maybe_pack[1] != SYNC_CHAR_2 {
                self.off += pos + 2;
                continue;
            }

            if maybe_pack.len() <= 5 {
                return None;
            }

            let pack_len: usize = u16::from_le_bytes([maybe_pack[4], maybe_pack[5]]).into();
            if (pack_len + 6 + 2) > self.buf.max_capacity() {
                self.off += pos + 2;
                return Some(Err(ParserError::OutOfMemory {
                    required_size: pack_len + 6 + 2,
                }));
            }
            if pack_len > usize::from(MAX_PAYLOAD_LEN) {
                self.off += pos + 2;
                continue;
            }
            if (pack_len + 6 + 2) > maybe_pack.len() {
                return None;
            }
            let (ck_a, ck_b) = ubx_checksum(&maybe_pack[2..(4 + pack_len + 2)]);

            let (expect_ck_a, expect_ck_b) =
                (maybe_pack[6 + pack_len], maybe_pack[6 + pack_len + 1]);
            if (ck_a, ck_b) != (expect_ck_a, expect_ck_b) {
                self.off += pos + 2;
                return Some(Err(ParserError::InvalidChecksum {
                    expect: u16::from_le_bytes([expect_ck_a, expect_ck_b]),
                    got: u16::from_le_bytes([ck_a, ck_b]),
                }));
            }
            let msg_data = &maybe_pack[6..(6 + pack_len)];
            let class_id = maybe_pack[2];
            let msg_id = maybe_pack[3];
            self.off += pos + 6 + pack_len + 2;
            return Some(match_packet(class_id, msg_id, msg_data));
        }
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ubx_packets::*;

    #[test]
    fn flb_clear() {
        let mut buf = [0; 16];
        let mut buf = FixedLinearBuffer::new(&mut buf);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(buf.len(), 7);
        buf.clear();
        assert_eq!(buf.len(), 0);
    }

    #[test]
    #[should_panic]
    fn flb_index_outside_range() {
        let mut buf = [0; 16];
        let mut buf = FixedLinearBuffer::new(&mut buf);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        let _ = buf[5..10];
    }

    #[test]
    fn flb_extend_outside_range() {
        let mut buf = [0; 16];
        let mut buf = FixedLinearBuffer::new(&mut buf);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(buf.len(), 16);
    }

    #[test]
    fn flb_drain() {
        let mut buf = [0; 16];
        let mut buf = FixedLinearBuffer::new(&mut buf);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);

        buf.drain(3);
        assert_eq!(buf.len(), 4);
        assert_eq!(&buf[0..buf.len()], &[4, 5, 6, 7]);

        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(buf.len(), 11);
        assert_eq!(&buf[0..buf.len()], &[4, 5, 6, 7, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn flb_drain_all() {
        let mut buf = [0; 16];
        let mut buf = FixedLinearBuffer::new(&mut buf);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);

        buf.drain(7);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn flb_find() {
        let mut buf = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut buf = FixedLinearBuffer::new(&mut buf);
        assert_eq!(buf.find(5), None);
        buf.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(buf.find(5), None);
        buf.extend_from_slice(&[5, 6, 7, 8]);
        assert_eq!(buf.find(5), Some(4));
    }

    #[test]
    fn parser_oom_processes_multiple_small_packets() {
        let packet = [0xb5, 0x62, 0x5, 0x1, 0x2, 0x0, 0x4, 0x5, 0x11, 0x38];

        let mut bytes = vec![];
        bytes.extend_from_slice(&packet);
        bytes.extend_from_slice(&packet);
        bytes.extend_from_slice(&packet);
        bytes.extend_from_slice(&packet);
        bytes.extend_from_slice(&packet);

        let mut buffer = [0; 10];
        let mut buffer = FixedLinearBuffer::new(&mut buffer);
        let mut parser = Parser::new(buffer);

        let mut it = parser.consume(&bytes);
        for i in 0..5 {
            match it.next() {
                Some(Ok(PacketRef::AckAck(_packet))) => {
                    // We're good
                    println!("Got packet {}...", i);
                }
                _ => assert!(false),
            }
        }
        assert!(it.next().is_none());
    }

    #[test]
    fn parser_oom_clears_buffer() {
        let bytes = CfgNav5Builder {
            mask: CfgNav5Params::DYN,
            dyn_model: CfgNav5DynModel::AirborneWithLess1gAcceleration,
            fix_mode: CfgNav5FixMode::Only3D,
            fixed_alt: 100.17,
            fixed_alt_var: 0.0017,
            min_elev_degrees: 17,
            pdop: 1.7,
            tdop: 1.7,
            pacc: 17,
            tacc: 17,
            static_hold_thresh: 2.17,
            dgps_time_out: 17,
            cno_thresh_num_svs: 17,
            cno_thresh: 17,
            static_hold_max_dist: 0x1717,
            utc_standard: CfgNav5UtcStandard::UtcChina,
            ..CfgNav5Builder::default()
        }
        .into_packet_bytes();

        let mut buffer = [0; 12];
        let mut buffer = FixedLinearBuffer::new(&mut buffer);
        let mut parser = Parser::new(buffer);

        {
            let mut it = parser.consume(&bytes[0..12]);
            match it.next() {
                Some(Err(ParserError::OutOfMemory { required_size })) => {
                    assert_eq!(required_size, bytes.len());
                }
                _ => {
                    assert!(false);
                }
            }
            assert!(it.next().is_none());
        }

        // Should now be empty, and we can parse a small packet
        let bytes = [0xb5, 0x62, 0x5, 0x1, 0x2, 0x0, 0x4, 0x5, 0x11, 0x38];

        {
            let mut it = parser.consume(&bytes);
            match it.next() {
                Some(Ok(PacketRef::AckAck(_packet))) => {
                    // We're good
                }
                Some(Err(e)) => {
                    println!("{:#?}", e);
                    println!("{}", bytes.len());
                    assert!(false);
                }
                _ => assert!(false),
            }
            assert!(it.next().is_none());
        }
    }

    #[test]
    fn parser_accepts_packet_array_underlying() {
        let bytes = CfgNav5Builder {
            mask: CfgNav5Params::DYN,
            dyn_model: CfgNav5DynModel::AirborneWithLess1gAcceleration,
            fix_mode: CfgNav5FixMode::Only3D,
            fixed_alt: 100.17,
            fixed_alt_var: 0.0017,
            min_elev_degrees: 17,
            pdop: 1.7,
            tdop: 1.7,
            pacc: 17,
            tacc: 17,
            static_hold_thresh: 2.17,
            dgps_time_out: 17,
            cno_thresh_num_svs: 17,
            cno_thresh: 17,
            static_hold_max_dist: 0x1717,
            utc_standard: CfgNav5UtcStandard::UtcChina,
            ..CfgNav5Builder::default()
        }
        .into_packet_bytes();

        let mut buffer = [0; 1024];
        let mut buffer = FixedLinearBuffer::new(&mut buffer);
        let mut parser = Parser::new(buffer);
        let mut it = parser.consume(&bytes);
        match it.next() {
            Some(Ok(PacketRef::CfgNav5(_packet))) => {
                // We're good
            }
            _ => {
                assert!(false);
            }
        }
        assert!(it.next().is_none());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parser_accepts_packet_vec_underlying() {
        let bytes = CfgNav5Builder {
            mask: CfgNav5Params::DYN,
            dyn_model: CfgNav5DynModel::AirborneWithLess1gAcceleration,
            fix_mode: CfgNav5FixMode::Only3D,
            fixed_alt: 100.17,
            fixed_alt_var: 0.0017,
            min_elev_degrees: 17,
            pdop: 1.7,
            tdop: 1.7,
            pacc: 17,
            tacc: 17,
            static_hold_thresh: 2.17,
            dgps_time_out: 17,
            cno_thresh_num_svs: 17,
            cno_thresh: 17,
            static_hold_max_dist: 0x1717,
            utc_standard: CfgNav5UtcStandard::UtcChina,
            ..CfgNav5Builder::default()
        }
        .into_packet_bytes();

        let mut parser = Parser::default();
        let mut it = parser.consume(&bytes);
        match it.next() {
            Some(Ok(PacketRef::CfgNav5(_packet))) => {
                // We're good
            }
            _ => {
                assert!(false);
            }
        }
        assert!(it.next().is_none());
    }

    #[test]
    #[cfg(feature = "std")]
    fn parser_accepts_multiple_packets() {
        let mut data = vec![];
        data.extend_from_slice(
            &CfgNav5Builder {
                pacc: 21,
                ..CfgNav5Builder::default()
            }
            .into_packet_bytes(),
        );
        data.extend_from_slice(
            &CfgNav5Builder {
                pacc: 18,
                ..CfgNav5Builder::default()
            }
            .into_packet_bytes(),
        );

        let mut parser = Parser::default();
        let mut it = parser.consume(&data);
        match it.next() {
            Some(Ok(PacketRef::CfgNav5(packet))) => {
                // We're good
                assert_eq!(packet.pacc(), 21);
            }
            _ => {
                assert!(false);
            }
        }
        match it.next() {
            Some(Ok(PacketRef::CfgNav5(packet))) => {
                // We're good
                assert_eq!(packet.pacc(), 18);
            }
            _ => {
                assert!(false);
            }
        }
        assert!(it.next().is_none());
    }
}

#[test]
fn test_max_payload_len() {
    assert!(MAX_PAYLOAD_LEN >= 1240);
}
