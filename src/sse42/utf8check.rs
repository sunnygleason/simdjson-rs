use crate::*;
#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

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

/*****************************/
#[cfg_attr(not(feature = "no-inline"), inline)]
fn push_last_byte_of_a_to_b(a: __m128i, b: __m128i) -> __m128i {
    unsafe {
        _mm_alignr_epi8(b, a, 16 - 1)
    }
}

#[cfg_attr(not(feature = "no-inline"), inline)]
fn push_last_2bytes_of_a_to_b(a: __m128i, b: __m128i) -> __m128i {
    unsafe {
        _mm_alignr_epi8(b, a, 16 - 2)
    }
}

// all byte values must be no larger than 0xF4
#[cfg_attr(not(feature = "no-inline"), inline)]
fn check_smaller_than_0xf4(current_bytes: __m128i, has_error: &mut __m128i) {
    // unsigned, saturates to 0 below max
    *has_error = unsafe {
        _mm_or_si128(
            *has_error,
            _mm_subs_epu8(current_bytes, _mm_set1_epi8(-12i8 /* 0xF4 */)),
        )
    };
}

macro_rules! nibbles_tbl {
    () => {
        _mm_setr_epi8(
            1, 1, 1, 1, 1, 1, 1, 1, // 0xxx (ASCII)
            0, 0, 0, 0,             // 10xx (continuation)
            2, 2,                   // 110x
            3,                      // 1110
            4,                      // 1111, next should be 0 (not checked here)
        )
    };
}

#[cfg_attr(not(feature = "no-inline"), inline)]
fn continuation_lengths(high_nibbles: __m128i) -> __m128i {
    unsafe {
        _mm_shuffle_epi8(
            nibbles_tbl!(),
            high_nibbles,
        )
    }
}

#[cfg_attr(not(feature = "no-inline"), inline)]
fn carry_continuations(initial_lengths: __m128i, previous_carries: __m128i) -> __m128i {
    unsafe {
        let right1: __m128i = _mm_subs_epu8(
            push_last_byte_of_a_to_b(previous_carries, initial_lengths),
            _mm_set1_epi8(1),
        );
        let sum: __m128i = _mm_add_epi8(initial_lengths, right1);
        let right2: __m128i = _mm_subs_epu8(
            push_last_2bytes_of_a_to_b(previous_carries, sum),
            _mm_set1_epi8(2),
        );
        _mm_add_epi8(sum, right2)
    }
}

#[cfg_attr(not(feature = "no-inline"), inline)]
fn check_continuations(initial_lengths: __m128i, carries: __m128i, has_error: &mut __m128i) {
    // overlap || underlap
    // carry > length && length > 0 || !(carry > length) && !(length > 0)
    // (carries > length) == (lengths > 0)
    unsafe {
        let overunder: __m128i = _mm_cmpeq_epi8(
            _mm_cmpgt_epi8(carries, initial_lengths),
            _mm_cmpgt_epi8(initial_lengths, _mm_setzero_si128()),
        );

        *has_error = _mm_or_si128(*has_error, overunder);
    }
}

// when 0xED is found, next byte must be no larger than 0x9F
// when 0xF4 is found, next byte must be no larger than 0x8F
// next byte must be continuation, ie sign bit is set, so signed < is ok
#[cfg_attr(not(feature = "no-inline"), inline)]
fn check_first_continuation_max(
    current_bytes: __m128i,
    off1_current_bytes: __m128i,
    has_error: &mut __m128i,
) {
    unsafe {
        let mask_ed: __m128i = _mm_cmpeq_epi8(
            off1_current_bytes,
            _mm_set1_epi8(static_cast_i8!(0xEDu8)),
        );
        let mask_f4: __m128i = _mm_cmpeq_epi8(
            off1_current_bytes,
            _mm_set1_epi8(static_cast_i8!(0xF4u8)),
        );

        let badfollow_ed: __m128i = _mm_and_si128(
            _mm_cmpgt_epi8(current_bytes, _mm_set1_epi8(static_cast_i8!(0x9Fu8))),
            mask_ed,
        );
        let badfollow_f4: __m128i = _mm_and_si128(
            _mm_cmpgt_epi8(current_bytes, _mm_set1_epi8(static_cast_i8!(0x8Fu8))),
            mask_f4,
        );

        *has_error = _mm_or_si128(
            *has_error,
            _mm_or_si128(badfollow_ed, badfollow_f4),
        );
    }
}

macro_rules! initial_mins_tbl {
    () => {
        _mm_setr_epi8(
            -128, -128, -128, -128, -128, -128,
            -128, -128, -128, -128, -128, -128, // 10xx => false
            -62 /* 0xC2 */, -128,               // 110x
            -31 /* 0xE1 */,                     // 1110
            -15 /*0xF1 */,                      // 1111
        )
    };
}

macro_rules! second_mins_tbl {
    () => {
        _mm_setr_epi8(
            -128, -128, -128, -128, -128, -128,
            -128, -128, -128, -128, -128, -128, // 10xx => false
            127, 127,                           // 110x => true
            -96 /* 0xA0 */,                     // 1110
            -112 /* 0x90 */,                    // 1111
        )
    };
}

// map off1_hibits => error condition
// hibits     off1    cur
// C       => < C2 && true
// E       => < E1 && < A0
// F       => < F1 && < 90
// else      false && false
#[cfg_attr(not(feature = "no-inline"), inline)]
fn check_overlong(
    current_bytes: __m128i,
    off1_current_bytes: __m128i,
    hibits: __m128i,
    previous_hibits: __m128i,
    has_error: &mut __m128i,
) {
    unsafe {
        let off1_hibits: __m128i = push_last_byte_of_a_to_b(previous_hibits, hibits);
        let initial_mins: __m128i = _mm_shuffle_epi8(
            initial_mins_tbl!(),
            off1_hibits,
        );

        let initial_under: __m128i = _mm_cmpgt_epi8(initial_mins, off1_current_bytes);

        let second_mins: __m128i = _mm_shuffle_epi8(
            second_mins_tbl!(),
            off1_hibits,
        );
        let second_under: __m128i = _mm_cmpgt_epi8(second_mins, current_bytes);
        *has_error = _mm_or_si128(
            *has_error,
            _mm_and_si128(initial_under, second_under)
        );
    }
}

pub struct ProcessedUtfBytes {
    rawbytes: __m128i,
    high_nibbles: __m128i,
    pub carried_continuations: __m128i,
}

impl Default for ProcessedUtfBytes {
    #[cfg_attr(not(feature = "no-inline"), inline)]
    fn default() -> Self {
        unsafe {
            ProcessedUtfBytes {
                rawbytes: _mm_setzero_si128(),
                high_nibbles: _mm_setzero_si128(),
                carried_continuations: _mm_setzero_si128(),
            }
        }
    }
}

#[cfg_attr(not(feature = "no-inline"), inline)]
fn count_nibbles(bytes: __m128i, answer: &mut ProcessedUtfBytes) {
    answer.rawbytes = bytes;
    answer.high_nibbles = unsafe {
        _mm_and_si128(
            _mm_srli_epi16(bytes, 4),
            _mm_set1_epi8(0x0F)
        )
    };
}

// check whether the current bytes are valid UTF-8
// at the end of the function, previous gets updated
#[cfg_attr(not(feature = "no-inline"), inline)]
pub fn check_utf8_bytes(
    current_bytes: __m128i,
    previous: &ProcessedUtfBytes,
    has_error: &mut __m128i,
) -> ProcessedUtfBytes {
    let mut pb = ProcessedUtfBytes::default();
    count_nibbles(current_bytes, &mut pb);

    check_smaller_than_0xf4(current_bytes, has_error);

    let initial_lengths: __m128i = continuation_lengths(pb.high_nibbles);

    pb.carried_continuations =
        carry_continuations(initial_lengths, previous.carried_continuations);

    check_continuations(initial_lengths, pb.carried_continuations, has_error);

    let off1_current_bytes: __m128i = push_last_byte_of_a_to_b(previous.rawbytes, pb.rawbytes);
    check_first_continuation_max(current_bytes, off1_current_bytes, has_error);

    check_overlong(
        current_bytes,
        off1_current_bytes,
        pb.high_nibbles,
        previous.high_nibbles,
        has_error,
    );
    pb
}
