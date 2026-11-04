// SPDX-License-Identifier: GPL-3.0-or-later

//! Keys and helpers shared by the `known_hosts` test files.

#![allow(dead_code, reason = "each test file uses only part of this")]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use fsm_sftp::known_hosts::HostKey;

/// The key most lines of the fixture hold.
pub const ED: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIILGfU4l4kLrfixMMTHNQu2sr90ZmqkD4Ujf0/HvxLll";
pub const ED_FP: &str = "SHA256:gvqDgoa56xT/v9j7ahDaI0FRLUCquR19BQupEU3gCdQ";
/// On no line of the fixture.
pub const ED2: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF+YoS2IZUR1zCUSiXw2bQXbhaXrqKzpdOSAePbVPXx2";
pub const ED2_FP: &str = "SHA256:VFQRcNhuzx/goro2nwJEr54WPrWDNllCOohUOCEqMkk";
pub const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDWHC1DFN7x2kNZ8anWrdmdjP4xGRSMs0OHvilj+HW6NKhone3GZrDW9nGNSvSV8L2ReQNAkBJmS1eHKEr5jfw0jbs5aZILbQ3Icoo9F5dnpk5zNXT1U9Epx0a7HJlRAO9RqcYu4pCOIoMJEC4aRDmYPfyWo2eQhksteZ/gk5coJ5RUQFp3ciO9quhpdvGmSp/6ziGkxIAkFVwUbxU/kFN813yc+CwG/sLsSN1nTv1etsFhHvKVmhz+kPdBtIbjwGDYjixD2yAbjlQqZ4qhAJi2r0A6AIpCuTZICBW4YtEyOugcSWYq9NxJLo+ktp275Aa2ZBtMghX9AnTijh6Cg4eQqZ9GjpVsv0rFJmCgMgKkdoh9PT+520X+KlkD5mPiKvRE4rTVAO8X9YscojWoMdw6Xa1y6m0WObA8wJ80BYSqPpe1RYjpRsMfF8ycXDCQZLzZH1V/YbMPQAimKQETQ3xIP/JYCCKbSQn5Ex5oOqh4ZuKjh0X+qRJfoj9FOalKdSM=";
pub const RSA_FP: &str = "SHA256:SstCy8fvaL7IvIYJw+mz1xsj+Ml41HqUR8HA4bxm9XI";
pub const P256: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBM8qcVCJNzcKs48GnmIXMjk3wFMtUUK1HrjLI9yrRTCBuskz3b9UZ5J29OzH+xOFnhVaPSASZA4GwzCOwRoj87g=";
pub const P256_FP: &str = "SHA256:FQAnuqVp3hRypGQd22eM5AugRsFkdVVyF3eAbvOrE1I";
pub const P384: &str = "ecdsa-sha2-nistp384 AAAAE2VjZHNhLXNoYTItbmlzdHAzODQAAAAIbmlzdHAzODQAAABhBMJA1PhnuxoRU+eZnIlWcGq7ITjEEdaYB/uvU9HCLTe/B7L6h99OXroZXYsu/sX/PsfqH8S7scLKXL4iRcmGOYJtQc100lu7O1jnIoLM9iC9Mp72EqElKKeC0RINYGQxkA==";
pub const P384_FP: &str = "SHA256:qVthLLU6CnBauiV40pWG1heNNcyQEDvTUxt1N2Cmheg";
/// On the fixture's `@revoked *` line only.
pub const P521: &str = "ecdsa-sha2-nistp521 AAAAE2VjZHNhLXNoYTItbmlzdHA1MjEAAAAIbmlzdHA1MjEAAACFBAGzhyuKPMknx6YR+jfmOYOa2icehF7gAi4S7U4m0CLZhGQQLsH2URoyE9pk6WDeIDHyFnbBUyLY8ufhhqkW3FAngAFjbx8YsC4L5iA/b1uD+cfvjas6tYARck3NEcQi6HRc0HvHK1CBa8FxHGCGacYtVzqIKij/esoxrxR7wfinTOslXA==";
pub const P521_FP: &str = "SHA256:yY1UfEm1ubc3AVGFAwqpMuxDp4BH1z4l/SyzarEB7g0";

/// Turns `type base64`, the form of a `.pub` file, into a key.
pub fn host_key(public: &str) -> HostKey {
    let (_, base64) = public.split_once(' ').unwrap();
    let base64 = base64.split_whitespace().next().unwrap();
    HostKey::from_wire(STANDARD.decode(base64).unwrap()).unwrap()
}

pub fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/known_hosts")
}

pub fn fixture() -> Vec<u8> {
    std::fs::read(fixture_path()).unwrap()
}
