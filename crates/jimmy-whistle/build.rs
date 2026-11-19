use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

const ROOT: &str = "https://huggingface.co/Cactus-Compute/cactus/resolve/f4ab2741203a282c6e7b89983de2b5e5b2e805a8/releases/4.1.4";
const INTEL_MAC: &str = "python/cactus_runtime-4.1.4-py3-none-macosx_11_0_x86_64.whl";

fn artifact(out: &Path, name: &str, expected: &str) -> Vec<u8> {
    let file = out.join(name.rsplit('/').next().expect("filename"));
    let provided = env::var_os("JIMMY_CACTUS_ARTIFACTS").map(PathBuf::from);
    let bytes = if file.is_file() {
        fs::read(&file).expect("read cached Cactus artifact")
    } else if let Some(provided) = provided {
        fs::read(provided.join(name)).expect("read local Cactus artifact")
    } else {
        ureq::get(format!("{ROOT}/{name}"))
            .call()
            .expect("download pinned Cactus artifact")
            .into_body()
            .with_config()
            .limit(5_000_000)
            .read_to_vec()
            .expect("read Cactus artifact")
    };
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        expected,
        "SHA-256 mismatch for {name}"
    );
    if !file.is_file() {
        let tmp = file.with_extension("tmp");
        fs::write(&tmp, &bytes).expect("write Cactus artifact");
        fs::rename(tmp, file).expect("install Cactus artifact");
    }
    bytes
}

fn main() {
    println!("cargo:rerun-if-env-changed=JIMMY_CACTUS_ARTIFACTS");
    println!("cargo:rerun-if-env-changed=JIMMY_CXX_LIB_DIR");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = env::var("TARGET").expect("TARGET");
    let (platform, hash) = match target.as_str() {
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => (
            "linux-x86_64",
            "a977d506ed9545369628cfe9fbf87747d73ab766ac376d8c1c098146485327c4",
        ),
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => (
            "linux-arm64",
            "31ee6ce7b0b1390fc8ffa6d242d2aa3cdae18b405d7d885d483cd79258016dd0",
        ),
        "aarch64-apple-darwin" => (
            "macos-arm64",
            "f00c3cf43e31db9399b7b1739146fa8ab293b6f3e71d7c7ab9c8a91042f8c5ef",
        ),
        "x86_64-pc-windows-gnullvm" => (
            "windows-x86_64",
            "013c73a2a68b0449be1a8e1a003f8d102da38e5460cccf0c62ea92fda37bc070",
        ),
        "aarch64-pc-windows-gnullvm" => (
            "windows-arm64",
            "d0a20fc405aa99e9b104b9ed4586781df06c2e01dfd51d35cb08731e9cb3939d",
        ),
        "x86_64-apple-darwin" => {
            let wheel = artifact(
                &out,
                INTEL_MAC,
                "bb04050273486582ef8484bd5c95cba3a43cb2db4510970b1f97cc6432f464cd",
            );
            let mut zip =
                zip::ZipArchive::new(std::io::Cursor::new(wheel)).expect("open Cactus wheel");
            let mut lib = Vec::new();
            zip.by_name("cactus/libcactus.dylib")
                .expect("Cactus dylib in wheel")
                .read_to_end(&mut lib)
                .expect("extract Cactus dylib");
            fs::write(out.join("libcactus.dylib"), lib).expect("write Cactus dylib");
            return;
        }
        _ => panic!(
            "Cactus 4.1.4 is unsupported for {target}. On Windows use the LLVM-MinGW gnullvm target."
        ),
    };
    artifact(&out, &format!("{platform}/libcactus.a"), hash);
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=cactus");
    if let Some(path) = env::var_os("JIMMY_CXX_LIB_DIR") {
        println!(
            "cargo:rustc-link-search=native={}",
            PathBuf::from(path).display()
        );
    } else if target.contains("linux-gnu") {
        // Debian/Ubuntu's libc++ development packages put the archives here.
        for version in (21..=24).rev() {
            let path = PathBuf::from(format!("/usr/lib/llvm-{version}/lib"));
            if path.join("libc++.a").is_file() {
                println!("cargo:rustc-link-search=native={}", path.display());
                break;
            }
        }
    }
    if target.ends_with("apple-darwin") {
        println!("cargo:rustc-link-lib=c++");
        println!("cargo:rustc-link-lib=framework=Accelerate");
    } else {
        println!("cargo:rustc-link-lib=static=c++");
        println!("cargo:rustc-link-lib=static=c++abi");
        if target.ends_with("gnullvm") {
            println!("cargo:rustc-link-lib=static=unwind");
        }
    }
}
