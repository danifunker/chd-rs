//! MAME CHD static Huffman **encoder** — a faithful port of the encode path in
//! [huffman.cpp](https://github.com/mamedev/mame/blob/master/src/lib/util/huffman.cpp)
//! (`huffman_context_base` / `huffman_encoder`). Produces output byte-identical to MAME for a
//! given histogram, so the `huff` codec and the V5 compressed-map writer match chdman.
//!
//! Algorithm ported from MAME `src/lib/util/huffman.cpp` and `bitstream.h`
//! (BSD-3-Clause, © Aaron Giles).
//!
//! The decode side ([`crate::huffman`]) reads the tree via `from_huffman_tree`, which
//! corresponds exactly to [`HuffEncoder::export_tree_huffman`] here.

/// MSB-first bit writer, a port of MAME's `bitstream_out` (`bitstream.h`). Writes into a
/// growable `Vec<u8>`; `flush` pads the final byte with zero bits and returns the bytes.
pub(crate) struct BitWriter {
    buffer: u32,
    bits: i32,
    out: Vec<u8>,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        BitWriter {
            buffer: 0,
            bits: 0,
            out: Vec::new(),
        }
    }

    /// Port of `bitstream_out::write`.
    pub(crate) fn write(&mut self, newbits: u32, numbits: i32) {
        if numbits <= 0 {
            return;
        }
        let mut newbits = newbits << (32 - numbits);
        let mut numbits = numbits;

        while self.bits + numbits >= 32 && numbits > 0 {
            while self.bits >= 8 {
                self.out.push((self.buffer >> 24) as u8);
                self.buffer <<= 8;
                self.bits -= 8;
            }
            if self.bits + numbits >= 32 {
                let rem = core::cmp::min(32 - self.bits, numbits);
                self.buffer |= newbits >> self.bits;
                self.bits += rem;
                // `rem` is in 1..=31 here, so the shift is always in range.
                newbits = newbits.wrapping_shl(rem as u32);
                numbits -= rem;
            }
        }

        if numbits <= 0 {
            return;
        }
        self.buffer |= newbits >> self.bits;
        self.bits += numbits;
    }

    /// Port of `bitstream_out::flush` — emit remaining bits (zero-padded), return the bytes.
    pub(crate) fn flush(mut self) -> Vec<u8> {
        while self.bits > 0 {
            self.out.push((self.buffer >> 24) as u8);
            self.buffer <<= 8;
            self.bits -= 8;
        }
        self.out
    }
}

const NULL_PARENT: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Node {
    parent: u32,
    weight: u32,
    bits: u32,
    numbits: u8,
}

impl Default for Node {
    fn default() -> Self {
        Node {
            parent: NULL_PARENT,
            weight: 0,
            bits: 0,
            numbits: 0,
        }
    }
}

/// Static Huffman encoder parameterized over the symbol count and max code length, mirroring
/// MAME's `huffman_encoder<NumCodes, MaxBits>`.
pub(crate) struct HuffEncoder {
    num_codes: usize,
    max_bits: u8,
    histo: Vec<u32>,
    nodes: Vec<Node>,
}

impl HuffEncoder {
    pub(crate) fn new(num_codes: usize, max_bits: u8) -> Self {
        HuffEncoder {
            num_codes,
            max_bits,
            histo: vec![0; num_codes],
            nodes: vec![Node::default(); num_codes * 2],
        }
    }

    pub(crate) fn histo_reset(&mut self) {
        self.histo.iter_mut().for_each(|h| *h = 0);
    }

    #[inline]
    pub(crate) fn histo_one(&mut self, data: u32) {
        self.histo[data as usize] += 1;
    }

    /// Port of `encode_one`: write the symbol's canonical code.
    #[inline]
    pub(crate) fn encode_one(&self, bitbuf: &mut BitWriter, data: u32) {
        let node = &self.nodes[data as usize];
        bitbuf.write(node.bits, node.numbits as i32);
    }

    /// Port of `compute_tree_from_histo`: binary-search the weight scaling so the resulting
    /// tree's max code length is `<= max_bits`, then assign canonical codes.
    pub(crate) fn compute_tree_from_histo(&mut self) -> Result<(), ()> {
        let sdatacount: u32 = self
            .histo
            .iter()
            .copied()
            .fold(0u32, |a, b| a.wrapping_add(b));

        let mut lowerweight: u32 = 0;
        let mut upperweight: u32 = sdatacount.wrapping_mul(2);
        loop {
            let curweight = (upperweight.wrapping_add(lowerweight)) / 2;
            let curmaxbits = self.build_tree(sdatacount, curweight);

            if curmaxbits <= self.max_bits {
                lowerweight = curweight;
                if curweight == sdatacount || upperweight.wrapping_sub(lowerweight) <= 1 {
                    break;
                }
            } else {
                upperweight = curweight;
            }
        }

        self.assign_canonical_codes()
    }

    /// Port of `build_tree`. Returns the maximum code length of the resulting tree.
    fn build_tree(&mut self, totaldata: u32, totalweight: u32) -> u8 {
        // reset all nodes (MAME memsets the leaves; internal nodes are written before read —
        // resetting both is equivalent and simpler).
        for n in self.nodes.iter_mut() {
            *n = Node::default();
        }

        // list of active node indices
        let mut list = vec![0u32; self.num_codes * 2];
        let mut listitems = 0usize;
        for curcode in 0..self.num_codes {
            if self.histo[curcode] != 0 {
                list[listitems] = curcode as u32;
                listitems += 1;
                self.nodes[curcode].bits = curcode as u32;
                let w = (self.histo[curcode] as u64 * totalweight as u64 / totaldata as u64) as u32;
                self.nodes[curcode].weight = if w == 0 { 1 } else { w };
            }
        }

        // sort by weight descending, tie-break by bits ascending (== MAME's qsort comparator;
        // `bits` is the unique code index for leaves, so this is a total order).
        let nodes = &self.nodes;
        list[..listitems].sort_by(|&a, &b| {
            let na = &nodes[a as usize];
            let nb = &nodes[b as usize];
            if nb.weight != na.weight {
                nb.weight.cmp(&na.weight)
            } else {
                na.bits.cmp(&nb.bits)
            }
        });

        // build the tree by repeatedly combining the two lowest-weight nodes
        let mut nextalloc = self.num_codes;
        while listitems > 1 {
            listitems -= 1;
            let n1 = list[listitems] as usize;
            listitems -= 1;
            let n0 = list[listitems] as usize;

            let newidx = nextalloc;
            nextalloc += 1;
            let newweight = self.nodes[n0].weight + self.nodes[n1].weight;
            self.nodes[newidx].parent = NULL_PARENT;
            self.nodes[newidx].weight = newweight;
            self.nodes[n0].parent = newidx as u32;
            self.nodes[n1].parent = newidx as u32;

            // insert newnode into list[0..listitems] keeping descending weight order
            let mut curitem = 0usize;
            while curitem < listitems {
                if newweight > self.nodes[list[curitem] as usize].weight {
                    break;
                }
                curitem += 1;
            }
            list.copy_within(curitem..listitems, curitem + 1);
            list[curitem] = newidx as u32;
            listitems += 1;
        }

        // compute the number of bits per leaf code (depth in the tree), track the max
        let mut maxbits = 0u8;
        for curcode in 0..self.num_codes {
            self.nodes[curcode].numbits = 0;
            self.nodes[curcode].bits = 0;
            if self.nodes[curcode].weight > 0 {
                let mut nb: u8 = 0;
                let mut cur = curcode;
                while self.nodes[cur].parent != NULL_PARENT {
                    cur = self.nodes[cur].parent as usize;
                    nb += 1;
                }
                if nb == 0 {
                    nb = 1;
                }
                self.nodes[curcode].numbits = nb;
                if nb > maxbits {
                    maxbits = nb;
                }
            }
        }
        maxbits
    }

    /// Port of `assign_canonical_codes`.
    fn assign_canonical_codes(&mut self) -> Result<(), ()> {
        let mut bithisto = [0u32; 33];
        for curcode in 0..self.num_codes {
            let nb = self.nodes[curcode].numbits;
            if nb > self.max_bits {
                return Err(());
            }
            if nb <= 32 {
                bithisto[nb as usize] += 1;
            }
        }

        let mut curstart = 0u32;
        for codelen in (1..=32usize).rev() {
            let nextstart = (curstart + bithisto[codelen]) >> 1;
            if codelen != 1 && nextstart * 2 != (curstart + bithisto[codelen]) {
                return Err(());
            }
            bithisto[codelen] = curstart;
            curstart = nextstart;
        }

        for curcode in 0..self.num_codes {
            let nb = self.nodes[curcode].numbits;
            if nb > 0 {
                self.nodes[curcode].bits = bithisto[nb as usize];
                bithisto[nb as usize] += 1;
            }
        }
        Ok(())
    }

    /// Port of `export_tree_huffman`: write the tree as a small-tree-coded RLE of code lengths,
    /// matching the decode side's `from_huffman_tree`.
    pub(crate) fn export_tree_huffman(&self, bitbuf: &mut BitWriter) -> Result<(), ()> {
        let mut rle_data: Vec<u8> = Vec::with_capacity(self.num_codes);
        let mut rle_lengths: Vec<u16> = Vec::new();
        let mut smallhuff = HuffEncoder::new(24, 6);

        let mut last: i32 = !0;
        let mut repcount: i32 = 0;

        for curcode in 0..self.num_codes {
            let newval = self.nodes[curcode].numbits as i32;
            if newval != last && repcount > 0 {
                if repcount == 1 {
                    let d = (last + 1) as u8;
                    smallhuff.histo_one(d as u32);
                    rle_data.push(d);
                } else {
                    smallhuff.histo_one(0);
                    rle_data.push(0);
                    rle_lengths.push((repcount - 2) as u16);
                }
            }
            if newval == last {
                repcount += 1;
            } else {
                let d = (newval + 1) as u8;
                smallhuff.histo_one(d as u32);
                rle_data.push(d);
                last = newval;
                repcount = 0;
            }
        }
        if repcount > 0 {
            if repcount == 1 {
                let d = (last + 1) as u8;
                smallhuff.histo_one(d as u32);
                rle_data.push(d);
            } else {
                smallhuff.histo_one(0);
                rle_data.push(0);
                rle_lengths.push((repcount - 2) as u16);
            }
        }

        smallhuff.compute_tree_from_histo()?;

        // first/last non-zero small-tree nodes
        let mut first_non_zero: i32 = 31;
        let mut last_non_zero: i32 = 0;
        for index in 1..smallhuff.num_codes {
            if smallhuff.nodes[index].numbits != 0 {
                if first_non_zero == 31 {
                    first_non_zero = index as i32;
                }
                last_non_zero = index as i32;
            }
        }
        first_non_zero = first_non_zero.min(8);

        bitbuf.write(smallhuff.nodes[0].numbits as u32, 3);
        bitbuf.write((first_non_zero - 1) as u32, 3);
        for index in first_non_zero..=last_non_zero {
            bitbuf.write(smallhuff.nodes[index as usize].numbits as u32, 3);
        }
        bitbuf.write(7, 3);

        // bits needed for an extended RLE count
        let mut temp = (self.num_codes - 9) as u32;
        let mut rlefullbits = 0u8;
        while temp != 0 {
            temp >>= 1;
            rlefullbits += 1;
        }

        // encode the RLE token stream
        let mut li = 0usize;
        for &data in &rle_data {
            smallhuff.encode_one(bitbuf, data as u32);
            if data == 0 {
                let count = rle_lengths[li];
                li += 1;
                if count < 7 {
                    bitbuf.write(count as u32, 3);
                } else {
                    bitbuf.write(7, 3);
                    bitbuf.write((count - 7) as u32, rlefullbits as i32);
                }
            }
        }
        Ok(())
    }
}

impl HuffEncoder {
    /// Port of `write_rle_tree_bits` (`huffman.cpp:468`) — write a run of `value` using the
    /// 1-escape RLE the tree export uses.
    fn write_rle_tree_bits(bitbuf: &mut BitWriter, value: i32, mut repcount: i32, numbits: i32) {
        while repcount > 0 {
            if value == 1 {
                // 1 is the escape code, so write it twice
                bitbuf.write(1, numbits);
                bitbuf.write(1, numbits);
                repcount -= 1;
            } else if repcount <= 2 {
                bitbuf.write(value as u32, numbits);
                repcount -= 1;
            } else {
                let cur_reps = (repcount - 3).min((1 << numbits) - 1);
                bitbuf.write(1, numbits);
                bitbuf.write(value as u32, numbits);
                bitbuf.write(cur_reps as u32, numbits);
                repcount -= cur_reps + 3;
            }
        }
    }

    /// Port of `export_tree_rle` (`huffman.cpp:204`): write the code lengths as a simple RLE
    /// bitstream (the inverse of the decoder's `from_tree_rle`). Used by the V5 map writer.
    pub(crate) fn export_tree_rle(&self, bitbuf: &mut BitWriter) {
        let numbits: i32 = if self.max_bits >= 16 {
            5
        } else if self.max_bits >= 8 {
            4
        } else {
            3
        };

        let mut lastval: i32 = !0;
        let mut repcount: i32 = 0;
        for curcode in 0..self.num_codes {
            let newval = self.nodes[curcode].numbits as i32;
            if newval == lastval {
                repcount += 1;
            } else {
                if repcount != 0 {
                    Self::write_rle_tree_bits(bitbuf, lastval, repcount, numbits);
                }
                lastval = newval;
                repcount = 1;
            }
        }
        Self::write_rle_tree_bits(bitbuf, lastval, repcount, numbits);
    }
}

/// Encode `source` with a 256-symbol / 16-bit-max static Huffman tree (the `huff` codec):
/// histogram → optimal tree → exported tree → symbols. Byte-identical to MAME's
/// `huffman_8bit_encoder::encode`.
pub(crate) fn encode_8bit(source: &[u8]) -> Result<Vec<u8>, ()> {
    let mut enc = HuffEncoder::new(256, 16);
    enc.histo_reset();
    for &b in source {
        enc.histo_one(b as u32);
    }
    enc.compute_tree_from_histo()?;

    let mut bitbuf = BitWriter::new();
    enc.export_tree_huffman(&mut bitbuf)?;
    for &b in source {
        enc.encode_one(&mut bitbuf, b as u32);
    }
    Ok(bitbuf.flush())
}
