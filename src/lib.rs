#![deny(warnings)]
#![cfg_attr(feature = "hints", feature(core_intrinsics))]
//! simdjson-rs is a rust port of the simejson c++ library. It follows
//! most of the design closely with a few exceptions to make it better
//! fit into the rust ecosystem.
//!
//! Note: by default rustc will compile for compatibility, not
//! performance, to take advantage of the simd part of simd json. You
//! have to use a native cpu target on a avx2 capable host system. An
//! example how to do this can be found in the `.cargo` directory on
//! [github](https://github.com/Licenser/simdjson-rs).
//!
//! ## Goals
//!
//! the goal of the rust port of simdjson is not to create a one to
//! one copy, but to integrate the principles of the c++ library into
//! a rust library that plays well with the rust ecosystem. As such
//! we provide both compatibility with serde as well as parsing to a
//! dom to manipulate data.
//!
//! ## Performance
//!
//! As a rule of thumb this library tries to get as close as posible
//! to the performance of the c++ implementation, but some of the
//! design decisions - such as parsing to a dom or a tape, weigh
//! ergonomics over performance. In other places Rust makes it harder
//! to achive the same level of performance.
//!
//! ## Safety
//!
//! this library uses unsafe all over the place, and while it leverages
//! quite a few test cases along with property based testing, please use
//! this library with caution.
//!
//!
//! ## Usage
//!
//! simdjson-rs offers two main entry points for usage:
//!
//! ### Values API
//!
//! The values API is a set of optimized DOM objects that allow parsed
//! json to JSON data that has no known variable structure. simdjson-rs
//! has two versions of this:
//!
//! **Borrowed Values**
//!
//! ```
//! use simd_json;
//! let mut d = br#"{"some": ["key", "value", 2]}"#.to_vec();
//! let v: simd_json::BorrowedValue = simd_json::to_borrowed_value(&mut d).unwrap();
//! ```
//!
//! **Owned Values**
//!
//! ```
//! use simd_json;
//! let mut d = br#"{"some": ["key", "value", 2]}"#.to_vec();
//! let v: simd_json::OwnedValue = simd_json::to_owned_value(&mut d).unwrap();
//! ```
//!
//! ### Serde Comaptible API
//!
//! ```
//! use simd_json;
//! use serde_json::Value;
//!
//! let mut d = br#"{"some": ["key", "value", 2]}"#.to_vec();
//! let v: Value = simd_json::serde::from_slice(&mut d).unwrap();
//! ```

#[cfg(feature = "serde_impl")]
extern crate serde as serde_ext;
#[cfg(feature = "serde_impl")]
pub mod serde;

mod charutils;
#[macro_use]
mod macros;
mod error;
mod numberparse;
mod parsedjson;
mod portability;
mod stage1;
mod stage2;
mod stringparse;
mod utf8check;
pub mod value;

use crate::numberparse::Number;
use crate::portability::*;
use crate::stringparse::*;
#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::mem;
use std::str;

pub use crate::error::{Error, ErrorType};
pub use crate::value::*;

const SIMDJSON_PADDING: usize = mem::size_of::<__m256i>();

pub type Result<T> = std::result::Result<T, Error>;

pub struct Deserializer<'de> {
    // This string starts with the input data and characters are truncated off
    // the beginning as data is parsed.
    input: &'de mut [u8],
    //data: Vec<u8>,
    strings: Vec<u8>,
    structural_indexes: Vec<u32>,
    idx: usize,
    counts: Vec<usize>,
    str_offset: usize,
    iidx: usize,
}

impl<'de> Deserializer<'de> {
    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn error(&self, error: ErrorType) -> Error {
        Error::new(self.idx, self.iidx, self.c() as char, error)
    }
    // By convention, `Deserializer` constructors are named like `from_xyz`.
    // That way basic use cases are satisfied by something like
    // `serde_json::from_str(...)` while advanced use cases that require a
    // deserializer can make one with `serde_json::Deserializer::from_str(...)`.
    pub fn from_slice(input: &'de mut [u8]) -> Result<Self> {
        // We have to pick an initial size of the structural indexes.
        // 6 is a heuristic that seems to work well for the benchmark
        // data and limit re-allocation frequency.

        let len = input.len();

        let buf_start: usize = input.as_ptr() as *const () as usize;
        let needs_relocation = (buf_start + input.len()) % page_size::get() < SIMDJSON_PADDING;

        let s1_result: std::result::Result<Vec<u32>, ErrorType> = if needs_relocation {
            let mut data: Vec<u8> = Vec::with_capacity(len + SIMDJSON_PADDING);
            unsafe {
                data.set_len(len + 1);
                data.as_mut_slice()
                    .get_unchecked_mut(0..len)
                    .clone_from_slice(input);
                *(data.get_unchecked_mut(len)) = 0;
                data.set_len(len);
                Deserializer::find_structural_bits(&data)
            }
        } else {
            unsafe { Deserializer::find_structural_bits(input) }
        };
        let structural_indexes = match s1_result {
            Ok(i) => i,
            Err(t) => {
                return Err(Error::generic(t));
            }
        };

        let counts = Deserializer::validate(input, &structural_indexes)?;

        let strings = Vec::with_capacity(len + SIMDJSON_PADDING);

        Ok(Deserializer {
            counts,
            structural_indexes,
            input,
            idx: 0,
            strings,
            str_offset: 0,
            iidx: 0,
        })
    }

    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn skip(&mut self) {
        self.idx += 1;
        self.iidx = unsafe { *self.structural_indexes.get_unchecked(self.idx) as usize };
    }

    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn c(&self) -> u8 {
        unsafe {
            *self
                .input
                .get_unchecked(*self.structural_indexes.get_unchecked(self.idx) as usize)
        }
    }

    // pull out the check so we don't need to
    // stry every time
    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn next_(&mut self) -> u8 {
        unsafe {
            self.idx += 1;
            self.iidx = *self.structural_indexes.get_unchecked(self.idx) as usize;
            *self.input.get_unchecked(self.iidx)
        }
    }

    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn count_elements(&self) -> usize {
        unsafe { *self.counts.get_unchecked(self.idx) }
    }

    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn parse_str_(&mut self) -> Result<&'de str> {
        // Add 1 to skip the initial "
        let idx = self.iidx + 1;
        let mut padding = [0u8; 32];
        //let mut read: usize = 0;

        // we include the terminal '"' so we know where to end
        // This is safe since we check sub's lenght in the range access above and only
        // create sub sliced form sub to `sub.len()`.

        let src: &[u8] = unsafe { &self.input.get_unchecked(idx..) };
        let mut src_i: usize = 0;
        let mut len = src_i;
        loop {
            let v: __m256i = if src.len() >= src_i + 32 {
                // This is safe since we ensure src is at least 32 wide
                #[allow(clippy::cast_ptr_alignment)]
                unsafe {
                    _mm256_loadu_si256(src.as_ptr().add(src_i) as *const __m256i)
                }
            } else {
                unsafe {
                    padding
                        .get_unchecked_mut(..src.len() - src_i)
                        .clone_from_slice(src.get_unchecked(src_i..));
                    // This is safe since we ensure src is at least 32 wide
                    #[allow(clippy::cast_ptr_alignment)]
                    _mm256_loadu_si256(padding.as_ptr() as *const __m256i)
                }
            };

            // store to dest unconditionally - we can overwrite the bits we don't like
            // later
            let bs_bits: u32 = unsafe {
                static_cast_u32!(_mm256_movemask_epi8(_mm256_cmpeq_epi8(
                    v,
                    _mm256_set1_epi8(b'\\' as i8)
                )))
            };
            let quote_mask = unsafe { _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'"' as i8)) };
            let quote_bits = unsafe { static_cast_u32!(_mm256_movemask_epi8(quote_mask)) };
            if (bs_bits.wrapping_sub(1) & quote_bits) != 0 {
                // we encountered quotes first. Move dst to point to quotes and exit
                // find out where the quote is...
                let quote_dist: u32 = trailingzeroes(u64::from(quote_bits)) as u32;

                ///////////////////////
                // Above, check for overflow in case someone has a crazy string (>=4GB?)
                // But only add the overflow check when the document itself exceeds 4GB
                // Currently unneeded because we refuse to parse docs larger or equal to 4GB.
                ////////////////////////

                // we advance the point, accounting for the fact that we have a NULl termination

                len += quote_dist as usize;
                unsafe {
                    let v = self.input.get_unchecked(idx..idx + len) as *const [u8] as *const str;
                    return Ok(&*v);
                }

                // we compare the pointers since we care if they are 'at the same spot'
                // not if they are the same value
            }
            if (quote_bits.wrapping_sub(1) & bs_bits) != 0 {
                // Move to the 'bad' character
                let bs_dist: u32 = trailingzeroes(u64::from(bs_bits));
                len += bs_dist as usize;
                src_i += bs_dist as usize;
                break;
            } else {
                // they are the same. Since they can't co-occur, it means we encountered
                // neither.
                src_i += 32;
                len += 32;
            }
        }

        let mut dst_i: usize = 0;
        let dst: &mut [u8] = &mut self.strings;

        loop {
            let v: __m256i = if src.len() >= src_i + 32 {
                // This is safe since we ensure src is at least 32 wide
                #[allow(clippy::cast_ptr_alignment)]
                unsafe {
                    _mm256_loadu_si256(src.as_ptr().add(src_i) as *const __m256i)
                }
            } else {
                unsafe {
                    padding
                        .get_unchecked_mut(..src.len() - src_i)
                        .clone_from_slice(src.get_unchecked(src_i..));
                    // This is safe since we ensure src is at least 32 wide
                    #[allow(clippy::cast_ptr_alignment)]
                    _mm256_loadu_si256(padding.as_ptr() as *const __m256i)
                }
            };

            #[allow(clippy::cast_ptr_alignment)]
            unsafe {
                _mm256_storeu_si256(dst.as_mut_ptr().add(dst_i) as *mut __m256i, v)
            };

            // store to dest unconditionally - we can overwrite the bits we don't like
            // later
            let bs_bits: u32 = unsafe {
                static_cast_u32!(_mm256_movemask_epi8(_mm256_cmpeq_epi8(
                    v,
                    _mm256_set1_epi8(b'\\' as i8)
                )))
            };
            let quote_mask = unsafe { _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'"' as i8)) };
            let quote_bits = unsafe { static_cast_u32!(_mm256_movemask_epi8(quote_mask)) };
            if (bs_bits.wrapping_sub(1) & quote_bits) != 0 {
                // we encountered quotes first. Move dst to point to quotes and exit
                // find out where the quote is...
                let quote_dist: u32 = trailingzeroes(u64::from(quote_bits)) as u32;

                ///////////////////////
                // Above, check for overflow in case someone has a crazy string (>=4GB?)
                // But only add the overflow check when the document itself exceeds 4GB
                // Currently unneeded because we refuse to parse docs larger or equal to 4GB.
                ////////////////////////

                // we advance the point, accounting for the fact that we have a NULl termination

                dst_i += quote_dist as usize;
                unsafe {
                    self.input
                        .get_unchecked_mut(idx + len..idx + len + dst_i)
                        .clone_from_slice(&self.strings.get_unchecked(..dst_i));
                    let v = self.input.get_unchecked(idx..idx + len + dst_i) as *const [u8]
                        as *const str;
                    self.str_offset += dst_i as usize;
                    return Ok(&*v);
                }

                // we compare the pointers since we care if they are 'at the same spot'
                // not if they are the same value
            }
            if (quote_bits.wrapping_sub(1) & bs_bits) != 0 {
                // find out where the backspace is
                let bs_dist: u32 = trailingzeroes(u64::from(bs_bits));
                let escape_char: u8 = unsafe { *src.get_unchecked(src_i + bs_dist as usize + 1) };
                // we encountered backslash first. Handle backslash
                if escape_char == b'u' {
                    // move src/dst up to the start; they will be further adjusted
                    // within the unicode codepoint handling code.
                    src_i += bs_dist as usize;
                    dst_i += bs_dist as usize;
                    let (o, s) = if let Ok(r) =
                        handle_unicode_codepoint(unsafe { src.get_unchecked(src_i..) }, unsafe {
                            dst.get_unchecked_mut(dst_i..)
                        }) {
                        r
                    } else {
                        return Err(self.error(ErrorType::InvlaidUnicodeCodepoint));
                    };
                    if o == 0 {
                        return Err(self.error(ErrorType::InvlaidUnicodeCodepoint));
                    };
                    // We moved o steps forword at the destiation and 6 on the source
                    src_i += s;
                    dst_i += o;
                } else {
                    // simple 1:1 conversion. Will eat bs_dist+2 characters in input and
                    // write bs_dist+1 characters to output
                    // note this may reach beyond the part of the buffer we've actually
                    // seen. I think this is ok
                    let escape_result: u8 =
                        unsafe { *ESCAPE_MAP.get_unchecked(escape_char as usize) };
                    if escape_result == 0 {
                        return Err(self.error(ErrorType::InvalidEscape));
                    }
                    unsafe {
                        *dst.get_unchecked_mut(dst_i + bs_dist as usize) = escape_result;
                    }
                    src_i += bs_dist as usize + 2;
                    dst_i += bs_dist as usize + 1;
                }
            } else {
                // they are the same. Since they can't co-occur, it means we encountered
                // neither.
                src_i += 32;
                dst_i += 32;
            }
        }
    }

    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn parse_number_root(&mut self, minus: bool) -> Result<Number> {
        let input = unsafe { &self.input.get_unchecked(self.iidx..) };
        let len = input.len();
        let mut copy = vec![0u8; len + SIMDJSON_PADDING];
        copy[len] = 0;
        unsafe {
            copy.as_mut_ptr().copy_from(input.as_ptr(), len);
        };
        self.parse_number_int(&copy, minus)
    }

    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn parse_number(&mut self, minus: bool) -> Result<Number> {
        let input = unsafe { &self.input.get_unchecked(self.iidx..) };
        let len = input.len();
        if len < SIMDJSON_PADDING {
            let mut copy = vec![0u8; len + SIMDJSON_PADDING];
            unsafe {
                copy.as_mut_ptr().copy_from(input.as_ptr(), len);
            };
            self.parse_number_int(&copy, minus)
        } else {
            self.parse_number_int(input, minus)
        }
    }

    #[cfg_attr(not(feature = "no-inline"), inline(always))]
    fn parse_number_(&mut self, minus: bool) -> Result<Number> {
        let input = unsafe { &self.input.get_unchecked(self.iidx..) };
        self.parse_number_int(input, minus)
    }
}

#[cfg(test)]
mod tests {
    use super::serde::from_slice;
    use super::{
        owned::to_value, owned::Map, owned::Value, to_borrowed_value, to_owned_value, Deserializer,
    };
    use halfbrown::HashMap;
    use proptest::prelude::*;
    use serde::Deserialize;
    use serde_json;

    #[test]
    fn count1() {
        let mut d = String::from("[]");
        let mut d = unsafe { d.as_bytes_mut() };
        let simd = Deserializer::from_slice(&mut d).expect("");
        assert_eq!(simd.counts[1], 0);
    }

    #[test]
    fn count2() {
        let mut d = String::from("[1]");
        let mut d = unsafe { d.as_bytes_mut() };
        let simd = Deserializer::from_slice(&mut d).expect("");
        assert_eq!(simd.counts[1], 1);
    }

    #[test]
    fn count3() {
        let mut d = String::from("[1,2]");
        let mut d = unsafe { d.as_bytes_mut() };
        let simd = Deserializer::from_slice(&mut d).expect("");
        assert_eq!(simd.counts[1], 2);
    }

    #[test]
    fn count4() {
        let mut d = String::from(" [ 1 , [ 3 ] , 2 ]");
        let mut d = unsafe { d.as_bytes_mut() };
        let simd = Deserializer::from_slice(&mut d).expect("");
        assert_eq!(simd.counts[1], 3);
        assert_eq!(simd.counts[4], 1);
    }

    #[test]
    fn count5() {
        let mut d = String::from("[[],null,null]");
        let mut d = unsafe { d.as_bytes_mut() };
        let simd = Deserializer::from_slice(&mut d).expect("");
        assert_eq!(simd.counts[1], 3);
        assert_eq!(simd.counts[2], 0);
    }

    #[test]
    fn empty() {
        let mut d = String::from("");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_simd = from_slice::<Value>(&mut d);
        let v_serde = serde_json::from_slice::<Value>(d);
        assert!(v_simd.is_err());
        assert!(v_serde.is_err());
    }

    #[test]
    fn bool_true() {
        let mut d = String::from("true");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };

        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(to_value(&mut d1), Ok(Value::from(true)));
    }

    #[test]
    fn bool_false() {
        let mut d = String::from("false");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(to_value(&mut d1), Ok(Value::from(false)));
        //assert!(false)
    }

    #[test]
    fn union() {
        let mut d = String::from("null");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(to_value(&mut d1), Ok(Value::Null));
    }

    #[test]
    fn int() {
        let mut d = String::from("42");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(to_value(&mut d1), Ok(Value::from(42)));
    }

    #[test]
    fn zero() {
        let mut d = String::from("0");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(to_value(&mut d1), Ok(Value::from(0)));
    }

    #[test]
    fn one() {
        let mut d = String::from("1");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(to_value(&mut d1), Ok(Value::from(1)));
    }

    #[test]
    fn minus_one() {
        let mut d = String::from("-1");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(to_value(&mut d1), Ok(Value::from(-1)));
    }

    #[test]
    fn float() {
        let mut d = String::from("23.0");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(to_value(&mut d1), Ok(Value::from(23.0)));
    }

    #[test]
    fn string() {
        let mut d = String::from(r#""snot""#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(to_value(&mut d1), Ok(Value::from("snot")));
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn lonely_quote() {
        let mut d = String::from(r#"""#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde = serde_json::from_slice::<serde_json::Value>(d).is_err();
        let v_simd = from_slice::<serde_json::Value>(&mut d).is_err();
        assert!(v_simd);
        assert!(v_serde);
    }

    #[test]
    fn lonely_quote1() {
        let mut d = String::from(r#"["]"#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde = serde_json::from_slice::<serde_json::Value>(d).is_err();
        let v_simd = from_slice::<serde_json::Value>(&mut d).is_err();
        assert!(v_simd);
        assert!(v_serde);
    }
    #[test]
    fn lonely_quote2() {
        let mut d = String::from(r#"[1, "]"#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde = serde_json::from_slice::<serde_json::Value>(d).is_err();
        let v_simd = from_slice::<serde_json::Value>(&mut d).is_err();
        assert!(v_simd);
        assert!(v_serde);
    }

    #[test]
    fn lonely_quote3() {
        let mut d = String::from(r#"{": 1}"#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde = serde_json::from_slice::<serde_json::Value>(d).is_err();
        let v_simd = from_slice::<serde_json::Value>(&mut d).is_err();
        assert!(v_simd);
        assert!(v_serde);
    }

    #[test]
    fn empty_string() {
        let mut d = String::from(r#""""#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(to_value(&mut d1), Ok(Value::from("")));
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn empty_array() {
        let mut d = String::from(r#"[]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("parse_serde");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("parse_simd");
        assert_eq!(to_value(&mut d1), Ok(Value::Array(vec![])));
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn malformed_array() {
        let mut d = String::from(r#"[["#);
        let mut d1 = d.clone();
        let mut d2 = d.clone();
        let mut d = unsafe { d.as_bytes_mut() };
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d2 = unsafe { d2.as_bytes_mut() };
        let v_serde: Result<serde_json::Value, _> = serde_json::from_slice(d);
        let v_simd_ov = to_owned_value(&mut d);
        let v_simd_bv = to_borrowed_value(&mut d1);
        let v_simd: Result<serde_json::Value, _> = from_slice(&mut d2);
        assert!(v_simd_ov.is_err());
        assert!(v_simd_bv.is_err());
        assert!(v_simd.is_err());
        assert!(v_serde.is_err());
    }

    #[test]
    fn double_array() {
        let mut d = String::from(r#"[[]]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("parse_serde");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("parse_simd");
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![Value::Array(vec![])]))
        );
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn null_null_array() {
        let mut d = String::from(r#"[[],null,null]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("parse_serde");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("parse_simd");
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![
                Value::Array(vec![]),
                Value::Null,
                Value::Null,
            ]))
        );
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn one_element_array() {
        let mut d = String::from(r#"["snot"]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![Value::from("snot")]))
        );
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn two_element_array() {
        let mut d = String::from(r#"["snot", "badger"]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![
                Value::from("snot"),
                Value::from("badger")
            ]))
        );
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn list() {
        let mut d = String::from(r#"[42, 23.0, "snot badger"]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![
                Value::from(42),
                Value::from(23.0),
                Value::from("snot badger")
            ]))
        );
    }

    #[test]
    fn nested_list1() {
        let mut d = String::from(r#"[42, [23.0, "snot"], "bad", "ger"]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![
                Value::from(42),
                Value::Array(vec![Value::from(23.0), Value::from("snot")]),
                Value::from("bad"),
                Value::from("ger")
            ]))
        );

        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn nested_list2() {
        let mut d = String::from(r#"[42, [23.0, "snot"], {"bad": "ger"}]"#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn utf8() {
        let mut d = String::from(r#""\u000e""#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, "\u{e}");
        // NOTE: serde is broken for this
        //assert_eq!(v_serde, "\u{e}");
        //assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn unicode() {
        let mut d = String::from(r#""¡\"""#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn odd_array() {
        let mut d = String::from("[{},null]");
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![Value::Object(Map::new()), Value::Null]))
        );
    }

    #[test]
    fn map2() {
        let mut d = String::from(r#"[{"\u0000":null}]"#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn null() {
        let mut d = String::from(r#"null"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        assert_eq!(to_value(&mut d1), Ok(Value::Null));
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
    }
    #[test]
    fn null_null() {
        let mut d = String::from(r#"[null, null]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![Value::Null, Value::Null,]))
        );
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn nested_null() {
        let mut d = String::from(r#"[[null, null]]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![Value::Array(vec![
                Value::Null,
                Value::Null,
            ])]))
        );

        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
    }

    #[test]
    fn nestednested_null() {
        let mut d = String::from(r#"[[[null, null]]]"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        assert_eq!(
            to_value(&mut d1),
            Ok(Value::Array(vec![Value::Array(vec![Value::Array(vec![
                Value::Null,
                Value::Null,
            ])])]))
        );
    }

    #[test]
    fn odd_array2() {
        let mut d = String::from("[[\"\\u0000\\\"\"]]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn odd_array3() {
        let mut d = String::from("[{\"\\u0000\\u0000\":null}]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn odd_array4() {
        let mut d = String::from("[{\"\\u0000𐀀a\":null}]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn float1() {
        let mut d = String::from("2.3250706903316115e307");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("serde_json");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    // We ignore this since serde is less percise on this test
    #[ignore]
    #[test]
    fn float2() {
        let mut d = String::from("-4.5512678569607477e306");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("serde_json");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn map0() {
        let mut d = String::from(r#"{"snot": "badger"}"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        let mut h = Map::new();
        h.insert("snot".into(), Value::from("badger"));
        assert_eq!(to_value(&mut d1), Ok(Value::Object(h)));
    }

    #[test]
    fn map1() {
        let mut d = String::from(r#"{"snot": "badger", "badger": "snot"}"#);
        let mut d1 = d.clone();
        let mut d1 = unsafe { d1.as_bytes_mut() };
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("");
        assert_eq!(v_simd, v_serde);
        let mut h = Map::new();
        h.insert("snot".into(), Value::from("badger"));
        h.insert("badger".into(), Value::from("snot"));
        assert_eq!(to_value(&mut d1), Ok(Value::Object(h)));
    }

    #[test]
    fn tpl1() {
        let mut d = String::from("[-65.613616999999977, 43.420273000000009]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: (f32, f32) = serde_json::from_slice(d).expect("serde_json");
        let v_simd: (f32, f32) = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn tpl2() {
        let mut d = String::from("[[-65.613616999999977, 43.420273000000009]]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: Vec<(f32, f32)> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: Vec<(f32, f32)> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn tpl3() {
        let mut d = String::from(
            "[[-65.613616999999977,43.420273000000009], [-65.613616999999977,43.420273000000009]]",
        );
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: Vec<(f32, f32)> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: Vec<(f32, f32)> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }
    #[test]
    fn tpl4() {
        let mut d = String::from("[[[-65.613616999999977,43.420273000000009]]]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: Vec<Vec<(f32, f32)>> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: Vec<Vec<(f32, f32)>> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }
    #[test]
    fn tpl5() {
        let mut d = String::from("[[[-65.613616999999977,43.420273000000009], [-65.613616999999977,43.420273000000009]]]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: Vec<Vec<(f32, f32)>> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: Vec<Vec<(f32, f32)>> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn tpl6() {
        let mut d = String::from("[[[[-65.613616999999977,43.420273000000009], [-65.613616999999977,43.420273000000009]]]]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: Vec<Vec<Vec<(f32, f32)>>> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: Vec<Vec<Vec<(f32, f32)>>> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn tpl7() {
        let mut d = String::from("[[[[-65.613616999999977,43.420273000000009], [-65.613616999999977,43.420273000000009]]]]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: Vec<Vec<Vec<[f32; 2]>>> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: Vec<Vec<Vec<[f32; 2]>>> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[derive(Deserialize, PartialEq, Debug)]
    struct Obj {
        a: u64,
        b: u64,
    }

    #[derive(Deserialize, PartialEq, Debug)]
    struct Obj1 {
        a: Obj,
    }

    #[test]
    fn obj() {
        let mut d = String::from(r#"{"a": 1, "b":1}"#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: Obj = serde_json::from_slice(d).expect("serde_json");
        let v_simd: Obj = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn obj2() {
        let mut d =
            String::from(r#"{"a": {"a": 1, "b":1}, "b": {"a": 1, "b":1}, "c": {"a": 1, "b": 1}}"#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: HashMap<String, Obj> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: HashMap<String, Obj> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn obj3() {
        let mut d = String::from(
            r#"{"c": {"a": {"a": 1, "b":1}, "b": {"a": 1, "b":1}, "c": {"a": 1, "b": 1}}}"#,
        );
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: HashMap<String, HashMap<String, Obj>> =
            serde_json::from_slice(d).expect("serde_json");
        let v_simd: HashMap<String, HashMap<String, Obj>> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn obj4() {
        let mut d = String::from(r#"{"c": {"a": {"a": 1, "b":1}}}"#);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: HashMap<String, Obj1> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: HashMap<String, Obj1> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn vecvec() {
        let mut d = String::from("[[[-65.613616999999977,43.420273000000009], [-65.613616999999977,43.420273000000009]], [[-65.613616999999977,43.420273000000009], [-65.613616999999977,43.420273000000009]]]");
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: Vec<Vec<(f32, f32)>> = serde_json::from_slice(d).expect("serde_json");
        let v_simd: Vec<Vec<(f32, f32)>> = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn crazy_string() {
        // there is unicode in here!
        let d = "\"𐀀𐀀  𐀀𐀀0 𐀀A\\u00000A0 A \\u000b\"";
        let mut d = String::from(d);
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: serde_json::Value = serde_json::from_slice(d).expect("serde_json");
        let v_simd: serde_json::Value = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    #[test]
    fn event() {
        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        pub struct CitmCatalog {
            pub area_names: HashMap<String, String>,
            pub audience_sub_category_names: HashMap<String, String>,
            pub block_names: HashMap<String, String>,
            pub events: HashMap<String, Event>,
        }
        pub type Id = u32;
        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        pub struct Event {
            pub description: (),
            pub id: Id,
            pub logo: Option<String>,
            pub name: String,
            pub sub_topic_ids: Vec<Id>,
            pub subject_code: (),
            pub subtitle: (),
            pub topic_ids: Vec<Id>,
        }

        let mut d = String::from(
            r#"
{
    "areaNames": {
        "205705993": "Arrière-scène central",
        "205705994": "1er balcon central",
        "205705995": "2ème balcon bergerie cour",
        "205705996": "2ème balcon bergerie jardin",
        "205705998": "1er balcon bergerie jardin",
        "205705999": "1er balcon bergerie cour",
        "205706000": "Arrière-scène jardin",
        "205706001": "Arrière-scène cour",
        "205706002": "2ème balcon jardin",
        "205706003": "2ème balcon cour",
        "205706004": "2ème Balcon central",
        "205706005": "1er balcon jardin",
        "205706006": "1er balcon cour",
        "205706007": "Orchestre central",
        "205706008": "Orchestre jardin",
        "205706009": "Orchestre cour",
        "342752287": "Zone physique secrète"
    },
    "audienceSubCategoryNames": {
        "337100890": "Abonné"
    },
    "blockNames": {},
  "events": {
    "138586341": {
      "description": null,
      "id": 138586341,
      "logo": null,
      "name": "30th Anniversary Tour",
      "subTopicIds": [
        337184269,
        337184283
      ],
      "subjectCode": null,
      "subtitle": null,
      "topicIds": [
        324846099,
        107888604
      ]
    },
    "138586345": {
      "description": null,
      "id": 138586345,
      "logo": "/images/UE0AAAAACEKo6QAAAAZDSVRN",
      "name": "Berliner Philharmoniker",
      "subTopicIds": [
        337184268,
        337184283,
        337184275
      ],
      "subjectCode": null,
      "subtitle": null,
      "topicIds": [
        324846099,
        107888604,
        324846100
      ]
    }
  }
}
"#,
        );
        let mut d = unsafe { d.as_bytes_mut() };
        let v_serde: CitmCatalog = serde_json::from_slice(d).expect("serde_json");
        let v_simd: CitmCatalog = from_slice(&mut d).expect("simd_json");
        assert_eq!(v_simd, v_serde)
    }

    // How much do we care about this, it's within the same range and
    // based on floating point math inprecisions during parsing.
    // Is this a real issue worth improving?
    #[test]
    fn silly_float1() {
        let v = Value::from(3.0901448042322017e305);
        let s = v.to_string();
        dbg!(&s);
        let mut bytes = s.as_bytes().to_vec();
        let parsed = to_owned_value(&mut bytes).expect("failed to parse gernated float");
        assert_eq!(v, parsed);
    }

    #[test]
    #[ignore]
    fn silly_float2() {
        let v = Value::from(-6.990585694841803e305);
        let s = v.to_string();
        dbg!(&s);
        let mut bytes = s.as_bytes().to_vec();
        let parsed = to_owned_value(&mut bytes).expect("failed to parse gernated float");
        assert_eq!(v, parsed);
    }

    //6.576692109929364e305
    fn arb_json() -> BoxedStrategy<String> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            // (-1.0e306f64..1.0e306f64).prop_map(|f| json!(f)), // The float parsing of simd and serde are too different
            any::<i64>().prop_map(|i| json!(i)),
            ".*".prop_map(Value::from),
        ];
        leaf.prop_recursive(
            8,   // 8 levels deep
            256, // Shoot for maximum size of 256 nodes
            10,  // We put up to 10 items per collection
            |inner| {
                prop_oneof![
                    // Take the inner strategy and make the two recursive cases.
                    prop::collection::vec(inner.clone(), 0..10).prop_map(|v| json!(v)),
                    prop::collection::hash_map(".*", inner, 0..10).prop_map(|m| json!(m)),
                ]
            },
        )
        .prop_map(|v| serde_json::to_string(&v).expect("").to_string())
        .boxed()
    }

    fn arb_json_value() -> BoxedStrategy<Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            //(-1.0e306f64..1.0e306f64).prop_map(|f| json!(f)), // damn you float!
            any::<i64>().prop_map(|i| json!(i)),
            ".*".prop_map(Value::from),
        ];
        leaf.prop_recursive(
            8,   // 8 levels deep
            256, // Shoot for maximum size of 256 nodes
            10,  // We put up to 10 items per collection
            |inner| {
                prop_oneof![
                    // Take the inner strategy and make the two recursive cases.
                    prop::collection::vec(inner.clone(), 0..10).prop_map(|v| json!(v)),
                    prop::collection::hash_map(".*", inner, 0..10).prop_map(|m| json!(m)),
                ]
            },
        )
        .boxed()
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            // Setting both fork and timeout is redundant since timeout implies
            // fork, but both are shown for clarity.
            fork: true,
            .. ProptestConfig::default()
        })]

        #[test]
        fn prop_json_encode_decode(val in arb_json_value()) {
            let mut encoded: Vec<u8> = Vec::new();
            let _ = val.write(&mut encoded);
            println!("{}", String::from_utf8(encoded.clone()).unwrap());
            let res = to_owned_value(&mut encoded).unwrap();
            assert_eq!(val, res);
        }

    }
    proptest! {
        #![proptest_config(ProptestConfig {
            // Setting both fork and timeout is redundant since timeout implies
            // fork, but both are shown for clarity.
            fork: true,
            .. ProptestConfig::default()
        })]

        #[test]
        fn prop_json(d in arb_json()) {
            if let Ok(v_serde) = serde_json::from_slice::<serde_json::Value>(&d.as_bytes()) {
                let mut d1 = d.clone();
                let d1 = unsafe{ d1.as_bytes_mut()};
                let v_simd_serde: serde_json::Value = from_slice(d1).expect("");
                // We add our own encoder in here.
                let mut d2 = v_simd_serde.to_string();
                let d2 = unsafe{ d2.as_bytes_mut()};
                let mut d3 = d.clone();
                let d3 = unsafe{ d3.as_bytes_mut()};
                assert_eq!(v_simd_serde, v_serde);
                let v_simd_owned = to_owned_value(d2);
                assert!(v_simd_owned.is_ok());
                let v_simd_borrowed = to_borrowed_value(d3);
                dbg!(&v_simd_borrowed);
                assert!(v_simd_borrowed.is_ok());
                assert_eq!(v_simd_owned.unwrap(), super::OwnedValue::from(v_simd_borrowed.unwrap()));
            }

        }

    }

    fn arb_junk() -> BoxedStrategy<Vec<u8>> {
        prop::collection::vec(any::<u8>(), 0..(1024 * 8)).boxed()
    }
    proptest! {
        #![proptest_config(ProptestConfig {
            // Setting both fork and timeout is redundant since timeout implies
            // fork, but both are shown for clarity.
            fork: true,
            .. ProptestConfig::default()
        })]
        #[test]
        fn prop_junk(d in arb_junk()) {
            let mut d1 = d.clone();
            let mut d2 = d.clone();
            let mut d3 = d.clone();

            let _ = from_slice::<serde_json::Value>(&mut d1);
            let _ = to_borrowed_value(&mut d2);
            let _ = to_owned_value(&mut d3);

        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            // Setting both fork and timeout is redundant since timeout implies
            // fork, but both are shown for clarity.
            fork: true,
            .. ProptestConfig::default()
        })]

        #[test]
        fn prop_string(d in "\\PC*") {
            let mut d1 = d.clone();
            let mut d1 = unsafe{ d1.as_bytes_mut()};
            let mut d2 = d.clone();
            let mut d2 = unsafe{ d2.as_bytes_mut()};
            let mut d3 = d.clone();
            let mut d3 = unsafe{ d3.as_bytes_mut()};
            let _ = from_slice::<serde_json::Value>(&mut d1);
            let _ = to_borrowed_value(&mut d2);
            let _ = to_owned_value(&mut d3);

        }
    }
}
