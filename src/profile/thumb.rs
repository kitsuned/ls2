use super::ReplyLayout;

// Decode only the Thumb encodings needed to recognize the transport's
// descriptor predicates and payload layouts. These are instruction fields
// from the Arm architecture, with register operands checked as data flow.
const MOVS_IMMEDIATE: u16 = 0x2000;
const CMP_IMMEDIATE: u16 = 0x2800;
const ADDS_IMMEDIATE: u16 = 0x3000;
const SUBS_IMMEDIATE: u16 = 0x3800;
const LDR_LITERAL: u16 = 0x4800;
const LDR_IMMEDIATE: u16 = 0x6800;
const BHI: u16 = 0xd800;
const BX_LR: u16 = 0x4700 | (14 << 3);
const MOVW: u16 = 0xf240;
const MOVT: u16 = 0xf2c0;

pub(super) fn descriptor_tags(code: &[u8], address: u64) -> Option<u64> {
    let mut thumb = Thumb::new(code);
    // Load the message header from the first argument, then load its type.
    let header = thumb.load_word(0, 28)?;
    let index = thumb.load_word(header, 8)?;
    let mut comparison = thumb.next()?;
    let first = if comparison & 0xff00 == SUBS_IMMEDIATE | (index << 8) {
        let base = usize::from(comparison & 0xff);
        comparison = thumb.next()?;
        base
    } else {
        0
    };
    if comparison & 0xff00 != CMP_IMMEDIATE | (index << 8) {
        return None;
    }
    let last = usize::from(comparison & 0xff);
    if first + last >= u64::BITS as usize {
        return None;
    }

    let control = thumb.next()?;
    if control & 0xff80 == BHI {
        // A forward conditional branch uses a PC two bytes beyond the cursor.
        let outside = thumb.offset + 2 + 2 * usize::from(control & 0x7f);
        if Thumb::at(code, outside).returns_bool()? {
            return None;
        }
        if thumb.peek()? == 0xe8df {
            return branch_table(thumb, index, first, last, outside);
        }
        let mask = shifted_bit_mask(thumb, address, index)?;
        return bounded_mask(mask, first, last);
    }

    // ITTTT LS applies the mask only when the type is within range.
    if first != 0 || control != if_then(9, 0b1111) {
        return None;
    }
    let mask = conditional_mask(thumb, index)?;
    bounded_mask(mask, first, last)
}

fn branch_table(
    mut thumb: Thumb<'_>,
    index: u16,
    first: usize,
    last: usize,
    outside: usize,
) -> Option<u64> {
    // TBB [pc, index] branches relative to the first byte of the table.
    thumb.expect(0xe8df)?;
    thumb.expect(0xf000 | index)?;
    let count = last + 1;
    if outside < thumb.offset + count {
        return None;
    }
    let table = thumb.code.get(thumb.offset..)?;
    table
        .get(..count)?
        .iter()
        .enumerate()
        .try_fold(0, |mask, (entry, offset)| {
            let target = 2 * usize::from(*offset);
            if target < count {
                return None;
            }
            let bit = u64::from(Thumb::at(table, target).returns_bool()?);
            Some(mask | (bit << (first + entry)))
        })
}

fn shifted_bit_mask(mut thumb: Thumb<'_>, address: u64, index: u16) -> Option<u32> {
    // Shift a one by the type index, intersect it with a loaded bitmask,
    // then normalize a nonzero result to the ABI's boolean return value.
    let one = thumb.next()?;
    let source = (one >> 8) & 7;
    if one & 0xf8ff != MOVS_IMMEDIATE | 1 || source == 0 || source == index || index == 0 {
        return None;
    }
    let mask_load = thumb.next()?;
    let mask = if mask_load & 0xff00 == LDR_LITERAL {
        // The literal's PC is word-aligned. Only the symbol's low bits affect
        // its alignment relative to the start of this in-memory slice.
        let alignment = (address & 3) as usize;
        let pc = (alignment + thumb.offset + 2) & !3;
        let literal = pc + 4 * usize::from(mask_load & 0xff) - alignment;
        u32::from_le_bytes(thumb.code.get(literal..literal + 4)?.try_into().ok()?)
    } else {
        u32::from(wide_move(mask_load, thumb.next()?, MOVW, 0)?)
    };

    thumb.expect(0xfa00 | source)?; // LSL.W shifted, source, index.
    let shift = thumb.next()?;
    let shifted = (shift >> 8) & 0xf;
    if shift != 0xf000 | (shifted << 8) | index || !(1..=7).contains(&shifted) {
        return None;
    }
    thumb.expect(0x4000 | (shifted << 3))?; // ANDS r0, shifted.
    thumb.expect(ADDS_IMMEDIATE)?; // Set the condition flags from r0.
    thumb.expect(if_then(1, 0b1000))?; // IT NE.
    thumb.returns_bool()?.then_some(mask)
}

fn conditional_mask(mut thumb: Thumb<'_>, index: u16) -> Option<u32> {
    if index == 0 {
        return None;
    }
    let low = thumb.next()?;
    let operand = thumb.next()?;
    let low = modified_immediate(low, operand, 0xf04f, 0)?; // MOV.W r0, #imm.
    let high = wide_move(thumb.next()?, thumb.next()?, MOVT, 0)?;
    thumb.expect(0x40c0 | (index << 3))?; // LSRS r0, index.
    let and = thumb.next()?;
    let operand = thumb.next()?;
    if modified_immediate(and, operand, 0xf000, 0)? != 1 {
        return None;
    }
    thumb.expect(if_then(8, 0b1000))?; // IT HI.
    (!thumb.returns_bool()?).then_some((u32::from(high) << 16) | (low & 0xffff))
}

fn bounded_mask(mask: u32, first: usize, last: usize) -> Option<u64> {
    let shift = (u32::BITS - 1).checked_sub(last.try_into().ok()?)?;
    Some(u64::from(mask & (u32::MAX >> shift)) << first)
}

fn if_then(condition: u16, mask: u16) -> u16 {
    0xbf00 | (condition << 4) | mask
}

fn wide_move(first: u16, second: u16, opcode: u16, register: u16) -> Option<u16> {
    if first & 0xfbf0 != opcode || second & 0x8f00 != register << 8 {
        return None;
    }
    Some(((first & 0xf) << 12) | immediate_field(first, second))
}

fn immediate_field(first: u16, second: u16) -> u16 {
    ((first & 0x0400) << 1) | ((second & 0x7000) >> 4) | (second & 0x00ff)
}

fn modified_immediate(first: u16, second: u16, opcode: u16, register: u16) -> Option<u32> {
    if first & 0xfbff != opcode || second & 0x8f00 != register << 8 {
        return None;
    }
    let imm = immediate_field(first, second);
    let byte = u32::from(imm & 0xff);
    match imm >> 8 {
        0 => Some(byte),
        1 if byte != 0 => Some(byte * 0x0001_0001),
        2 if byte != 0 => Some(byte * 0x0100_0100),
        3 if byte != 0 => Some(byte * 0x0101_0101),
        1..=3 => None,
        _ => Some(u32::from(0x80 | (imm & 0x7f)).rotate_right(u32::from(imm >> 7))),
    }
}

pub(super) fn payload_layout(code: &[u8]) -> Option<ReplyLayout> {
    if code.len() % 2 != 0 {
        return None;
    }
    let mut thumb = Thumb::new(code);
    let mut typed = false;
    let mut sized = false;
    while let Some(first) = thumb.next() {
        if first >= 0xe800 {
            let second = thumb.next()?;
            // BIC pointer, pointer, #7 clears alignment bits. A word load
            // from just beyond the MIME terminator reads the payload length.
            let pointer = first & 0xf;
            if modified_immediate(first, second, 0xf020 | pointer, pointer) == Some(7) {
                let mut following = thumb;
                sized |= matches!(
                    (following.next(), following.next()),
                    (Some(load), Some(operand))
                        if load & 0xfff0 == 0xf8d0 && operand & 0x0fff == 1
                );
            }
        } else if first & 0xfe00 == 0x1a00 {
            // SUBS length, length, mime_length and ADD payload, mime_length.
            let length = first & 7;
            let mime_length = (first >> 6) & 7;
            typed |= (first >> 3) & 7 == length
                && matches!(thumb.peek(), Some(add)
                    if add & 0xff78 == 0x4400 | (mime_length << 3));
        }
    }
    match (typed, sized) {
        (true, false) => Some(ReplyLayout::Typed),
        (false, true) => Some(ReplyLayout::Sized),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Thumb<'a> {
    code: &'a [u8],
    offset: usize,
}

impl<'a> Thumb<'a> {
    fn new(code: &'a [u8]) -> Self {
        Self::at(code, 0)
    }

    fn at(code: &'a [u8], offset: usize) -> Self {
        Self { code, offset }
    }

    fn peek(self) -> Option<u16> {
        let bytes = self.code.get(self.offset..self.offset + 2)?;
        Some(u16::from_le_bytes(bytes.try_into().ok()?))
    }

    fn next(&mut self) -> Option<u16> {
        let instruction = self.peek()?;
        self.offset += 2;
        Some(instruction)
    }

    fn expect(&mut self, opcode: u16) -> Option<()> {
        (self.next()? == opcode).then_some(())
    }

    fn load_word(&mut self, base: u16, byte_offset: u16) -> Option<u16> {
        let opcode = LDR_IMMEDIATE | ((byte_offset / 4) << 6) | (base << 3);
        let instruction = self.next()?;
        (instruction & !7 == opcode).then_some(instruction & 7)
    }

    fn returns_bool(mut self) -> Option<bool> {
        let value = match self.next()? {
            MOVS_IMMEDIATE => false,
            instruction if instruction == MOVS_IMMEDIATE | 1 => true,
            _ => return None,
        };
        self.expect(BX_LR)?;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_wire_numbers_from_a_thumb_branch_table() {
        // A synthetic predicate uses r1 for the header and r2 for the type.
        let instructions = [
            0x69c1,
            0x688a,
            0x3a08,
            0x2a01,
            0xd802,
            0xe8df,
            0xf002,
            0x0301, // False for 8, true for 9.
            MOVS_IMMEDIATE,
            BX_LR,
            MOVS_IMMEDIATE | 1,
            BX_LR,
        ];
        let mut code: Vec<u8> = instructions
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(descriptor_tags(&code, 0), Some(1 << 9));
        code.swap(14, 15);
        assert_eq!(descriptor_tags(&code, 0), Some(1 << 8));
        code[14] = 0;
        assert_eq!(descriptor_tags(&code, 0), None);
    }

    #[test]
    fn recognizes_payload_layouts_with_different_registers() {
        // SUBS r1, r1, r2; ADD r3, r2.
        let typed: Vec<u8> = [0x1a89, 0x4413]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        // BIC r5, r5, #7; LDR.W r4, [r6, #1].
        let sized: Vec<u8> = [0xf025, 0x0507, 0xf8d6, 0x4001]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();

        assert_eq!(payload_layout(&typed), Some(ReplyLayout::Typed));
        assert_eq!(payload_layout(&sized), Some(ReplyLayout::Sized));
        assert_eq!(payload_layout(&sized[..6]), None);
        assert_eq!(payload_layout(&[typed, sized].concat()), None);
    }
}
