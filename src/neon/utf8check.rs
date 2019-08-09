
use crate::neon::intrinsics::*;
use std::mem;

/*
 * legal utf-8 byte sequence
 * http://www.unicode.org/versions/Unicode6.0.0/ch03.pdf - page 94
 *
 *  Code Points        1st       2s       3s       4s
 * U+0000..U+007F     00..7F
 * U+0080..U+07FF     C2..DF   80..BF
 * U+0800..U+0FFF     E0       A0..BF   80..BF
 * U+1000..U+CFFF     E1..EC   80..BF   80..BF
 * U+D000..U+D7FF     ED       80..9F   80..BF
 * U+E000..U+FFFF     EE..EF   80..BF   80..BF
 * U+10000..U+3FFFF   F0       90..BF   80..BF   80..BF
 * U+40000..U+FFFFF   F1..F3   80..BF   80..BF   80..BF
 * U+100000..U+10FFFF F4       80..8F   80..BF   80..BF
 *
 */

// all byte values must be no larger than 0xF4
#[cfg_attr(not(feature = "no-inline"), inline)]
unsafe fn check_smaller_than_0xf4(current_bytes: int8x16_t, has_error: &mut int8x16_t) {
    // unsigned, saturates to 0 below max
    *has_error =
        vorrq_s8(
            *has_error,
            vreinterpretq_s8_u8(vqsubq_u8(
                vreinterpretq_u8_s8(current_bytes), vdupq_n_u8(0xF4))));
}

#[cfg_attr(not(feature = "no-inline"), inline)]
unsafe fn continuation_lengths(high_nibbles: int8x16_t) -> int8x16_t {
    let NIBBLES : int8x16_t = int8x16_t::new(
        1, 1, 1, 1, 1, 1, 1, 1, // 0xxx (ASCII)
        0, 0, 0, 0,             // 10xx (continuation)
        2, 2,                   // 110x
        3,                      // 1110
        4,                      // 1111, next should be 0 (not checked here)
    );

    vqtbl1q_s8(NIBBLES, vreinterpretq_u8_s8(high_nibbles))
}

#[cfg_attr(not(feature = "no-inline"), inline)]
unsafe fn carry_continuations(initial_lengths: int8x16_t, previous_carries: int8x16_t) -> int8x16_t {
    let right1 : int8x16_t = vreinterpretq_s8_u8(vqsubq_u8(
        vreinterpretq_u8_s8(vextq_s8(previous_carries, initial_lengths, 16 - 1)),
        vdupq_n_u8(1)));
    let sum : int8x16_t = vaddq_s8(initial_lengths, right1);

    let right2 : int8x16_t = vreinterpretq_s8_u8(
        vqsubq_u8(vreinterpretq_u8_s8(vextq_s8(previous_carries, sum, 16 - 2)),
                  vdupq_n_u8(2)));

    vaddq_s8(sum, right2)
}

#[cfg_attr(not(feature = "no-inline"), inline)]
unsafe fn check_continuations(initial_lengths: int8x16_t, carries: int8x16_t, has_error: &mut int8x16_t) {
    // overlap || underlap
    // carry > length && length > 0 || !(carry > length) && !(length > 0)
    // (carries > length) == (lengths > 0)

    // FIXME: was vceqq_u8 ?
    let overunder : int8x16_t = vceqq_s8(
                                    vcgtq_s8(carries, initial_lengths),
                                    vcgtq_s8(initial_lengths, vdupq_n_s8(0)));

    *has_error = vorrq_s8(*has_error, overunder);
}

// when 0xED is found, next byte must be no larger than 0x9F
// when 0xF4 is found, next byte must be no larger than 0x8F
// next byte must be continuation, ie sign bit is set, so signed < is ok
#[cfg_attr(not(feature = "no-inline"), inline)]
unsafe fn check_first_continuation_max(
    current_bytes: int8x16_t,
    off1_current_bytes: int8x16_t,
    has_error: &mut int8x16_t,
) {
    let maskED : int8x16_t = vceqq_s8(off1_current_bytes, vdupq_n_s8(0xED));
    let maskF4 : int8x16_t = vceqq_s8(off1_current_bytes, vdupq_n_s8(0xF4));

    // FIXME: was vandq_u8?
    let badfollowED : int8x16_t =
        vandq_s8(vcgtq_s8(current_bytes, vdupq_n_s8(0x9F)), maskED);
    let badfollowF4 : int8x16_t =
        vandq_s8(vcgtq_s8(current_bytes, vdupq_n_s8(0x8F)), maskF4);

    *has_error = vorrq_s8(
        *has_error, vorrq_s8(badfollowED, badfollowF4));
}

// map off1_hibits => error condition
// hibits     off1    cur
// C       => < C2 && true
// E       => < E1 && < A0
// F       => < F1 && < 90
// else      false && false
#[cfg_attr(not(feature = "no-inline"), inline)]
unsafe fn check_overlong(
    current_bytes: int8x16_t,
    off1_current_bytes: int8x16_t,
    hibits: int8x16_t,
    previous_hibits: int8x16_t,
    has_error: &mut int8x16_t,
) {
    let _initial_mins : int8x16_t = int8x16_t::new(
        -128,         -128, -128, -128, -128, -128,
        -128,         -128, -128, -128, -128, -128, // 10xx => false
        0xC2, -128,                         // 110x
        0xE1,                               // 1110
        0xF1,
    );

    let _second_mins : int8x16_t = int8x16_t::new(
        -128,         -128, -128, -128, -128, -128,
        -128,         -128, -128, -128, -128, -128, // 10xx => false
        127,          127,                          // 110x => true
        0xA0,                               // 1110
        0x90,
    );

    let off1_hibits : int8x16_t = vextq_s8(previous_hibits, hibits, 16 - 1);
    let initial_mins : int8x16_t =
        vqtbl1q_s8(_initial_mins, vreinterpretq_u8_s8(off1_hibits));

    let initial_under : int8x16_t = vcgtq_s8(initial_mins, off1_current_bytes);

    let second_mins : int8x16_t =
        vqtbl1q_s8(_second_mins, vreinterpretq_u8_s8(off1_hibits));

    let second_under : int8x16_t = vcgtq_s8(second_mins, current_bytes);

    *has_error = vorrq_s8(
        *has_error, vandq_s8(initial_under, second_under));
}

pub struct ProcessedUtfBytes {
    rawbytes: int8x16_t,
    high_nibbles: int8x16_t,
    pub carried_continuations: int8x16_t,
}

impl Default for ProcessedUtfBytes {
    #[cfg_attr(not(feature = "no-inline"), inline)]
    fn default() -> Self {
        ProcessedUtfBytes {
            rawbytes: vdupq_n_s8(0x00),
            high_nibbles: vdupq_n_s8(0x00),
            carried_continuations: vdupq_n_s8(0x00),
        }
    }
}

#[cfg_attr(not(feature = "no-inline"), inline)]
unsafe fn count_nibbles(bytes: int8x16_t, answer: &mut ProcessedUtfBytes) {
    answer.rawbytes = bytes;
    answer.high_nibbles = vreinterpretq_s8_u8(mem::transmute(vshrq_n_u8(vreinterpretq_u8_s8(bytes), 4)));
}

// check whether the current bytes are valid UTF-8
// at the end of the function, previous gets updated
#[cfg_attr(not(feature = "no-inline"), inline)]
pub fn check_utf8_bytes(
    current_bytes: int8x16_t,
    previous: &ProcessedUtfBytes,
    has_error: &mut int8x16_t,
) -> ProcessedUtfBytes {
    let mut pb = ProcessedUtfBytes::default();
    unsafe {
        count_nibbles(current_bytes, &mut pb);

        check_smaller_than_0xf4(current_bytes, has_error);

        let initial_lengths: int8x16_t = continuation_lengths(pb.high_nibbles);

        pb.carried_continuations =
            carry_continuations(initial_lengths, previous.carried_continuations);

        check_continuations(initial_lengths, pb.carried_continuations, has_error);

        let off1_current_bytes : int8x16_t =
            vextq_s8(previous.rawbytes, pb.rawbytes, 16 - 1);

        check_first_continuation_max(current_bytes, off1_current_bytes, has_error);

        check_overlong(current_bytes, off1_current_bytes, pb.high_nibbles,
                       previous.high_nibbles, has_error);
    }
    pb
}
