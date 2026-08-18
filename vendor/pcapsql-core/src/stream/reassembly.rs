use std::collections::{BTreeMap, HashMap};

use super::Direction;

/// A TCP segment waiting to be reassembled.
#[derive(Debug, Clone)]
pub struct Segment {
    pub seq: u32,
    pub data: Vec<u8>,
    pub frame_number: u64,
    pub timestamp: i64,
}

/// A gap in the sequence space (missing data).
#[derive(Debug, Clone)]
pub struct SequenceGap {
    pub start_seq: u32,
    pub end_seq: u32,
}

/// Buffer for one direction of a TCP stream.
#[derive(Debug)]
pub struct StreamBuffer {
    /// Next expected sequence number.
    expected_seq: u32,
    /// Initial sequence number (from SYN).
    initial_seq: Option<u32>,
    /// Whether initial_seq was set from a SYN (definitive) vs inferred.
    initial_seq_from_syn: bool,
    /// Out-of-order segments waiting to be reassembled.
    pending: BTreeMap<u32, Segment>,
    /// Contiguous reassembled data ready for parsing.
    reassembled: Vec<u8>,
    /// Detected gaps (missing segments).
    gaps: Vec<SequenceGap>,
    /// Statistics.
    pub segment_count: u32,
    pub retransmit_count: u32,
    pub out_of_order_count: u32,
    /// Sequence bytes skipped past an unfillable gap.
    pub bytes_skipped: u64,
    /// How many maintenance ticks the stream has been stuck behind a gap.
    stuck_ticks: u8,
    /// FIN received.
    pub fin_received: bool,
}

impl StreamBuffer {
    pub fn new() -> Self {
        Self {
            expected_seq: 0,
            initial_seq: None,
            initial_seq_from_syn: false,
            pending: BTreeMap::new(),
            reassembled: Vec::new(),
            gaps: Vec::new(),
            segment_count: 0,
            retransmit_count: 0,
            out_of_order_count: 0,
            bytes_skipped: 0,
            stuck_ticks: 0,
            fin_received: false,
        }
    }

    /// Set the initial sequence number (from SYN).
    pub fn set_initial_seq(&mut self, seq: u32) {
        self.initial_seq = Some(seq);
        self.initial_seq_from_syn = true;
        self.expected_seq = seq.wrapping_add(1); // SYN consumes one seq
    }

    /// Fast path for in-order segment - avoids intermediate Vec allocation.
    /// Returns true if the segment was handled (in-order with no pending segments).
    /// Returns false if the segment needs to be handled by the slow path.
    ///
    /// This copies data directly into the reassembled buffer, avoiding the
    /// intermediate `Segment { data: data.to_vec(), ... }` allocation that
    /// the slow path requires.
    #[inline]
    pub fn add_inorder_data(
        &mut self,
        seq: u32,
        data: &[u8],
        _frame_number: u64,
        _timestamp: i64,
    ) -> bool {
        // Fast path: segment is in-order and no pending segments exist
        // This avoids allocating an intermediate Vec for the Segment struct
        if self.initial_seq.is_some() && seq == self.expected_seq && self.pending.is_empty() {
            self.segment_count += 1;
            self.reassembled.extend_from_slice(data);
            self.expected_seq = seq_add(seq, data.len());
            self.stuck_ticks = 0;
            true
        } else {
            false
        }
    }

    /// Add a segment to the buffer (slow path - takes ownership).
    pub fn add_segment(&mut self, segment: Segment) {
        self.segment_count += 1;

        // If we haven't seen a SYN, use first segment's seq
        if self.initial_seq.is_none() {
            self.initial_seq = Some(segment.seq);
            self.expected_seq = segment.seq;
        }

        let seg_end = seq_add(segment.seq, segment.data.len());

        // Check for retransmission (segment starts before expected)
        if seq_lt(segment.seq, self.expected_seq) {
            // Special case: mid-stream mode and we received an earlier segment
            // This means we started in the middle and now got an earlier packet
            if !self.initial_seq_from_syn && seq_lt(segment.seq, self.initial_seq.unwrap()) {
                // Move current reassembled data to pending and reset
                let old_initial = self.initial_seq.unwrap();
                let old_data = std::mem::take(&mut self.reassembled);
                if !old_data.is_empty() {
                    self.pending.insert(
                        old_initial,
                        Segment {
                            seq: old_initial,
                            data: old_data,
                            frame_number: 0,
                            timestamp: 0,
                        },
                    );
                }
                // Set new initial and process this segment
                self.initial_seq = Some(segment.seq);
                self.expected_seq = segment.seq;
                self.add_segment_inner(segment);
                return;
            }

            // Check if it's fully before expected (pure retransmit)
            if seq_le(seg_end, self.expected_seq) {
                self.retransmit_count += 1;
                return;
            }
            // Partial overlap - trim the beginning
            let overlap = self.expected_seq.wrapping_sub(segment.seq) as usize;
            if overlap < segment.data.len() {
                let trimmed = Segment {
                    seq: self.expected_seq,
                    data: segment.data[overlap..].to_vec(),
                    frame_number: segment.frame_number,
                    timestamp: segment.timestamp,
                };
                self.add_segment_inner(trimmed);
            }
            return;
        }

        self.add_segment_inner(segment);
    }

    fn add_segment_inner(&mut self, segment: Segment) {
        // In-order segment
        if segment.seq == self.expected_seq {
            self.reassembled.extend_from_slice(&segment.data);
            self.expected_seq = seq_add(segment.seq, segment.data.len());
            self.stuck_ticks = 0;

            // Check if pending segments can now be added
            self.flush_pending();
        } else if seq_lt(self.expected_seq, segment.seq) {
            // Out of order - buffer it behind a gap.
            self.out_of_order_count += 1;
            self.note_gap(self.expected_seq, segment.seq);
            self.pending.insert(segment.seq, segment);
        }
    }

    /// Try to flush pending segments that are now in order.
    fn flush_pending(&mut self) {
        while let Some((&seq, _)) = self.pending.first_key_value() {
            if seq == self.expected_seq {
                let segment = self.pending.remove(&seq).unwrap();
                self.reassembled.extend_from_slice(&segment.data);
                self.expected_seq = seq_add(segment.seq, segment.data.len());
                self.stuck_ticks = 0;
            } else if seq_lt(seq, self.expected_seq) {
                // Retransmit that arrived late, remove it
                self.pending.remove(&seq);
            } else {
                // Gap - can't continue
                break;
            }
        }
    }

    /// Record a gap once, without spamming an entry per late segment.
    fn note_gap(&mut self, start: u32, end: u32) {
        if self.gaps.last().map(|g| g.start_seq) != Some(start) {
            self.record_gap(start, end);
        }
    }

    /// Whether the stream is blocked: everything contiguous has been consumed
    /// yet pending data waits behind a hole that has not been filled.
    fn is_stuck(&self) -> bool {
        self.reassembled.is_empty()
            && self
                .pending
                .first_key_value()
                .is_some_and(|(&seq, _)| seq_lt(self.expected_seq, seq))
    }

    /// Count one maintenance tick and report whether the stream has now been
    /// stuck behind the same gap long enough to give up on the missing bytes.
    /// Passive capture (`SNIFF | RECV_ONLY`) never sees a retransmit of a packet
    /// it dropped, so a genuine hole would otherwise stall the stream forever.
    pub fn tick_stuck(&mut self, threshold: u8) -> bool {
        if self.is_stuck() {
            self.stuck_ticks = self.stuck_ticks.saturating_add(1);
            self.stuck_ticks >= threshold
        } else {
            self.stuck_ticks = 0;
            false
        }
    }

    /// Skip forward past an unfillable gap to the next buffered segment so the
    /// stream can make progress again. Returns the number of sequence bytes
    /// skipped, or `None` when the stream is not actually stuck.
    pub fn recover_stuck_gap(&mut self) -> Option<u64> {
        if !self.is_stuck() {
            return None;
        }
        let target = *self.pending.first_key_value()?.0;
        let skipped = u64::from(target.wrapping_sub(self.expected_seq));
        self.note_gap(self.expected_seq, target);
        self.expected_seq = target;
        self.bytes_skipped = self.bytes_skipped.saturating_add(skipped);
        self.stuck_ticks = 0;
        self.flush_pending();
        Some(skipped)
    }

    /// Get contiguous reassembled data.
    pub fn get_contiguous(&self) -> &[u8] {
        &self.reassembled
    }

    /// Consume bytes from the reassembled buffer (after successful parse).
    pub fn consume(&mut self, bytes: usize) {
        if bytes > 0 && bytes <= self.reassembled.len() {
            self.reassembled.drain(..bytes);
        }
    }

    /// Check if stream is complete (no gaps, FIN received).
    pub fn is_complete(&self) -> bool {
        self.fin_received && self.pending.is_empty()
    }

    /// Get current gaps in the stream.
    pub fn gaps(&self) -> &[SequenceGap] {
        &self.gaps
    }

    /// Record a gap when we detect missing data.
    pub fn record_gap(&mut self, start: u32, end: u32) {
        self.gaps.push(SequenceGap {
            start_seq: start,
            end_seq: end,
        });
    }

    /// Get number of bytes available for parsing.
    pub fn available(&self) -> usize {
        self.reassembled.len()
    }

    /// Total payload bytes retained by this stream, including out-of-order
    /// segments waiting for a gap to be filled.
    pub fn buffered_bytes(&self) -> usize {
        self.reassembled.len()
            + self
                .pending
                .values()
                .map(|segment| segment.data.len())
                .sum::<usize>()
    }

    /// Get the number of gaps in the stream.
    pub fn gap_count(&self) -> u32 {
        self.gaps.len() as u32
    }

    /// Get the segment count.
    pub fn segment_count(&self) -> u32 {
        self.segment_count
    }

    /// Get the retransmission count.
    pub fn retransmit_count(&self) -> u32 {
        self.retransmit_count
    }

    /// Get the out-of-order segment count.
    pub fn out_of_order_count(&self) -> u32 {
        self.out_of_order_count
    }
}

impl Default for StreamBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Key for stream buffer lookup.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct StreamKey {
    pub connection_id: u64,
    pub direction: Direction,
}

/// TCP stream reassembler.
pub struct TcpReassembler {
    streams: HashMap<StreamKey, StreamBuffer>,
}

impl TcpReassembler {
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
        }
    }

    /// Get or create a stream buffer.
    pub fn get_or_create(&mut self, connection_id: u64, direction: Direction) -> &mut StreamBuffer {
        let key = StreamKey {
            connection_id,
            direction,
        };
        self.streams.entry(key).or_default()
    }

    /// Add a segment to the appropriate stream.
    pub fn add_segment(
        &mut self,
        connection_id: u64,
        direction: Direction,
        seq: u32,
        data: &[u8],
        frame_number: u64,
        timestamp: i64,
    ) {
        if data.is_empty() {
            return; // No payload
        }

        let buffer = self.get_or_create(connection_id, direction);

        // Try fast path first (no allocation for in-order segments)
        if !buffer.add_inorder_data(seq, data, frame_number, timestamp) {
            // Fall back to slow path with copy
            buffer.add_segment(Segment {
                seq,
                data: data.to_vec(),
                frame_number,
                timestamp,
            });
        }
    }

    /// Advance every stuck stream that has stayed blocked behind a gap for at
    /// least `threshold` maintenance ticks, skipping past the missing bytes.
    /// Returns one entry per recovered stream: `(connection_id, direction,
    /// bytes_skipped)`, so the caller can report a possibly-missed message.
    pub fn recover_stuck_gaps(&mut self, threshold: u8) -> Vec<(u64, Direction, u64)> {
        let mut recovered = Vec::new();
        for (key, buffer) in self.streams.iter_mut() {
            if buffer.tick_stuck(threshold) {
                if let Some(skipped) = buffer.recover_stuck_gap() {
                    recovered.push((key.connection_id, key.direction, skipped));
                }
            }
        }
        recovered
    }

    /// Get contiguous data for a stream.
    pub fn get_contiguous(&self, connection_id: u64, direction: Direction) -> &[u8] {
        let key = StreamKey {
            connection_id,
            direction,
        };
        self.streams
            .get(&key)
            .map(|b| b.get_contiguous())
            .unwrap_or(&[])
    }

    /// Consume bytes from a stream.
    pub fn consume(&mut self, connection_id: u64, direction: Direction, bytes: usize) {
        let key = StreamKey {
            connection_id,
            direction,
        };
        if let Some(buffer) = self.streams.get_mut(&key) {
            buffer.consume(bytes);
        }
    }

    /// Mark FIN received for a stream.
    pub fn mark_fin(&mut self, connection_id: u64, direction: Direction) {
        let key = StreamKey {
            connection_id,
            direction,
        };
        if let Some(buffer) = self.streams.get_mut(&key) {
            buffer.fin_received = true;
        }
    }

    /// Check if a stream is complete.
    pub fn is_complete(&self, connection_id: u64, direction: Direction) -> bool {
        let key = StreamKey {
            connection_id,
            direction,
        };
        self.streams
            .get(&key)
            .map(|b| b.is_complete())
            .unwrap_or(false)
    }

    /// Remove a stream (connection closed).
    pub fn remove(&mut self, connection_id: u64) -> usize {
        let before = self.buffered_bytes();
        self.streams.retain(|k, _| k.connection_id != connection_id);
        before.saturating_sub(self.buffered_bytes())
    }

    /// Bytes currently retained for a single connection.
    pub fn connection_buffered_bytes(&self, connection_id: u64) -> usize {
        self.streams
            .iter()
            .filter(|(key, _)| key.connection_id == connection_id)
            .map(|(_, buffer)| buffer.buffered_bytes())
            .sum()
    }

    /// Total payload bytes retained by all connections.
    pub fn buffered_bytes(&self) -> usize {
        self.streams
            .values()
            .map(StreamBuffer::buffered_bytes)
            .sum()
    }

    /// Get stream statistics.
    pub fn stats(&self, connection_id: u64, direction: Direction) -> Option<StreamStats> {
        let key = StreamKey {
            connection_id,
            direction,
        };
        self.streams.get(&key).map(|b| StreamStats {
            segment_count: b.segment_count,
            retransmit_count: b.retransmit_count,
            out_of_order_count: b.out_of_order_count,
            gap_count: b.gaps.len() as u32,
            bytes_skipped: b.bytes_skipped,
            bytes_available: b.available(),
        })
    }
}

impl Default for TcpReassembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Stream statistics.
#[derive(Debug, Clone)]
pub struct StreamStats {
    pub segment_count: u32,
    pub retransmit_count: u32,
    pub out_of_order_count: u32,
    pub gap_count: u32,
    pub bytes_skipped: u64,
    pub bytes_available: usize,
}

// Sequence number comparison helpers
fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn seq_le(a: u32, b: u32) -> bool {
    a == b || seq_lt(a, b)
}

fn seq_add(a: u32, n: usize) -> u32 {
    a.wrapping_add(n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test 1: In-order segment reassembly
    #[test]
    fn test_in_order_reassembly() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 1, 0);
        reassembler.add_segment(1, Direction::ToServer, 1005, b" World", 2, 1);

        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"Hello World");
    }

    // Test 2: Out-of-order segment reordering
    #[test]
    fn test_out_of_order_reordering() {
        let mut reassembler = TcpReassembler::new();

        // Arrive out of order
        reassembler.add_segment(1, Direction::ToServer, 1005, b" World", 2, 1);
        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 1, 0);

        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"Hello World");
    }

    // Test 3: Retransmission detection
    #[test]
    fn test_retransmission_detection() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 1, 0);
        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 2, 1); // Retransmit

        let stats = reassembler.stats(1, Direction::ToServer).unwrap();
        assert_eq!(stats.retransmit_count, 1);

        // Data should appear only once
        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"Hello");
    }

    // Test 4: Sequence number wraparound
    #[test]
    fn test_sequence_wraparound() {
        let mut reassembler = TcpReassembler::new();

        // Near max u32
        let near_max = u32::MAX - 2;
        reassembler.add_segment(1, Direction::ToServer, near_max, b"ABC", 1, 0);
        reassembler.add_segment(
            1,
            Direction::ToServer,
            near_max.wrapping_add(3),
            b"DEF",
            2,
            1,
        );

        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"ABCDEF");
    }

    // Test 5: Gap detection
    #[test]
    fn test_gap_detection() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 1, 0);
        // Skip 1005-1009, add segment starting at 1010
        reassembler.add_segment(1, Direction::ToServer, 1010, b"World", 2, 1);

        // Only "Hello" should be available (gap before "World")
        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"Hello");

        let stats = reassembler.stats(1, Direction::ToServer).unwrap();
        assert_eq!(stats.out_of_order_count, 1);
    }

    // Test 6: Overlapping segments (partial retransmit)
    #[test]
    fn test_overlapping_segments() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 1, 0);
        // Overlapping: starts at 1003, overlaps "lo"
        reassembler.add_segment(1, Direction::ToServer, 1003, b"loWorld", 2, 1);

        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"HelloWorld");
    }

    // Test 7: Zero-length payload
    #[test]
    fn test_zero_length_payload() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 1, 0);
        reassembler.add_segment(1, Direction::ToServer, 1005, b"", 2, 1); // Empty
        reassembler.add_segment(1, Direction::ToServer, 1005, b"World", 3, 2);

        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"HelloWorld");
    }

    // Test 8: Consume advances buffer
    #[test]
    fn test_consume() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"HelloWorld", 1, 0);

        // Consume "Hello"
        reassembler.consume(1, Direction::ToServer, 5);

        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"World");
    }

    // Test 9: Get contiguous returns available data
    #[test]
    fn test_get_contiguous() {
        let mut reassembler = TcpReassembler::new();

        // No data yet
        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert!(data.is_empty());

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Test", 1, 0);

        let data = reassembler.get_contiguous(1, Direction::ToServer);
        assert_eq!(data, b"Test");
    }

    // Test 10: Multiple streams per connection
    #[test]
    fn test_multiple_streams() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Request", 1, 0);
        reassembler.add_segment(1, Direction::ToClient, 2000, b"Response", 2, 1);

        assert_eq!(
            reassembler.get_contiguous(1, Direction::ToServer),
            b"Request"
        );
        assert_eq!(
            reassembler.get_contiguous(1, Direction::ToClient),
            b"Response"
        );
    }

    // Test 11: Buffer limits (memory usage)
    #[test]
    fn test_stats() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 1, 0);
        reassembler.add_segment(1, Direction::ToServer, 1010, b"World", 2, 1); // Gap

        let stats = reassembler.stats(1, Direction::ToServer).unwrap();
        assert_eq!(stats.segment_count, 2);
        assert_eq!(stats.out_of_order_count, 1);
        assert_eq!(stats.bytes_available, 5); // Only "Hello"
    }

    // Test 12: is_complete detection
    #[test]
    fn test_is_complete() {
        let mut reassembler = TcpReassembler::new();

        reassembler.add_segment(1, Direction::ToServer, 1000, b"Hello", 1, 0);
        assert!(!reassembler.is_complete(1, Direction::ToServer));

        reassembler.mark_fin(1, Direction::ToServer);
        assert!(reassembler.is_complete(1, Direction::ToServer));
    }

    // Test 13: Fast path (add_inorder_data) works correctly
    #[test]
    fn test_inorder_fast_path() {
        let mut buffer = StreamBuffer::new();

        // First segment - no initial_seq yet, fast path returns false
        assert!(!buffer.add_inorder_data(1000, b"Hello", 1, 0));

        // Set up initial state via slow path
        buffer.add_segment(Segment {
            seq: 1000,
            data: b"Hello".to_vec(),
            frame_number: 1,
            timestamp: 0,
        });
        assert_eq!(buffer.get_contiguous(), b"Hello");
        assert_eq!(buffer.segment_count, 1);

        // Second segment - should use fast path
        assert!(buffer.add_inorder_data(1005, b" World", 2, 1));
        assert_eq!(buffer.get_contiguous(), b"Hello World");
        assert_eq!(buffer.segment_count, 2);

        // Third segment - also fast path
        assert!(buffer.add_inorder_data(1011, b"!", 3, 2));
        assert_eq!(buffer.get_contiguous(), b"Hello World!");
        assert_eq!(buffer.segment_count, 3);
    }

    // Test 14: Fast path skipped when pending segments exist
    #[test]
    fn test_inorder_fast_path_skipped_with_pending() {
        let mut buffer = StreamBuffer::new();

        // Set up initial state
        buffer.add_segment(Segment {
            seq: 1000,
            data: b"Hello".to_vec(),
            frame_number: 1,
            timestamp: 0,
        });

        // Add out-of-order segment (creates pending)
        buffer.add_segment(Segment {
            seq: 1010,
            data: b"World".to_vec(),
            frame_number: 3,
            timestamp: 2,
        });

        // Now even if we have in-order data, fast path should return false
        // because there are pending segments
        assert!(!buffer.add_inorder_data(1005, b"_____", 2, 1));
    }

    // Test 15: A stuck gap is skipped only after the tick threshold, and the
    // stream then makes progress past the missing bytes.
    #[test]
    fn recovers_a_stream_stuck_behind_an_unfillable_gap() {
        let mut reassembler = TcpReassembler::new();
        reassembler.add_segment(1, Direction::ToClient, 1000, b"HEAD", 1, 0);
        // 1004..1010 is lost; the tail arrives out of order and waits.
        reassembler.add_segment(1, Direction::ToClient, 1010, b"TAIL", 2, 1);
        // Consume the contiguous head so the stream is now blocked on the hole.
        reassembler.consume(1, Direction::ToClient, 4);
        assert_eq!(reassembler.get_contiguous(1, Direction::ToClient), b"");

        // One tick is not enough - a merely reordered segment must get a chance.
        assert!(reassembler.recover_stuck_gaps(2).is_empty());
        assert_eq!(reassembler.get_contiguous(1, Direction::ToClient), b"");

        // On the second tick it gives up and skips to the buffered tail.
        let recovered = reassembler.recover_stuck_gaps(2);
        assert_eq!(recovered, vec![(1, Direction::ToClient, 6)]);
        assert_eq!(reassembler.get_contiguous(1, Direction::ToClient), b"TAIL");

        let stats = reassembler.stats(1, Direction::ToClient).unwrap();
        assert_eq!(stats.bytes_skipped, 6);
        assert!(stats.gap_count >= 1);
    }

    // Test 16: A gap that fills before the threshold is never skipped.
    #[test]
    fn a_reordered_segment_is_not_skipped() {
        let mut reassembler = TcpReassembler::new();
        reassembler.add_segment(1, Direction::ToClient, 1000, b"HEAD", 1, 0);
        reassembler.add_segment(1, Direction::ToClient, 1008, b"TAIL", 2, 1);
        reassembler.consume(1, Direction::ToClient, 4);

        // First tick observes the gap but waits.
        assert!(reassembler.recover_stuck_gaps(2).is_empty());
        // The missing middle finally arrives and the stream is whole again.
        reassembler.add_segment(1, Direction::ToClient, 1004, b"MIDD", 3, 2);
        assert_eq!(reassembler.get_contiguous(1, Direction::ToClient), b"MIDDTAIL");
        // The stuck counter was reset, so nothing is ever skipped.
        assert!(reassembler.recover_stuck_gaps(2).is_empty());
        let stats = reassembler.stats(1, Direction::ToClient).unwrap();
        assert_eq!(stats.bytes_skipped, 0);
    }

    #[test]
    fn buffered_bytes_include_pending_and_are_released() {
        let mut reassembler = TcpReassembler::new();
        reassembler.add_segment(1, Direction::ToServer, 1000, b"hello", 1, 0);
        reassembler.add_segment(1, Direction::ToServer, 1010, b"world", 2, 1);
        reassembler.add_segment(1, Direction::ToClient, 2000, b"reply", 3, 2);

        assert_eq!(reassembler.connection_buffered_bytes(1), 15);
        assert_eq!(reassembler.buffered_bytes(), 15);
        assert_eq!(reassembler.remove(1), 15);
        assert_eq!(reassembler.buffered_bytes(), 0);
    }
}
