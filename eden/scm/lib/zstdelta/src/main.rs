/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod zstdelta;

use crate::zstdelta::{apply, diff};
use std::env::args;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;

fn read(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    File::open(path)
        .expect("open")
        .read_to_end(&mut buf)
        .expect("read");
    buf
}

fn main() {
    let args: Vec<_> = args().skip(1).collect();
    if args.len() < 3 {
        println!("Usage: zstdelta -c base data > delta\n       zstdelta -d base delta > data\n");
        exit(1);
    }
    let base = read(&PathBuf::from(&args[1]));
    let data = read(&PathBuf::from(&args[2]));
    let out = if args[0] == "-c" {
        diff(&base, &data).expect("diff")
    } else {
        apply(&base, &data).expect("apply")
    };

    io::stdout().write_all(&out).expect("write");
}
