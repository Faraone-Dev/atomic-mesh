use std::path::PathBuf;

fn main() {
    let cpp_dir: PathBuf = ["../../cpp"].iter().collect();

    cc::Build::new()
        .cpp(true)
        .file(cpp_dir.join("src/engine.cpp"))
        .file(cpp_dir.join("src/ringbuf.cpp"))
        .include(cpp_dir.join("include"))
        .std("c++17")
        .opt_level(3)
        .warnings(false)
        .define("NDEBUG", None)
        .flag_if_supported("-march=native")
        .flag_if_supported("-mavx2")
        .flag_if_supported("-ffast-math")
        .flag_if_supported("-flto")
        // MSVC equivalents
        .flag_if_supported("/arch:AVX2")
        .flag_if_supported("/fp:fast")
        .compile("atomic_hotpath_cpp");

    println!("cargo:rerun-if-changed=../../cpp/include/hotpath.h");
    println!("cargo:rerun-if-changed=../../cpp/src/engine.cpp");
    println!("cargo:rerun-if-changed=../../cpp/src/orderbook.cpp");
    println!("cargo:rerun-if-changed=../../cpp/src/microprice.cpp");
    println!("cargo:rerun-if-changed=../../cpp/src/signal.cpp");
    println!("cargo:rerun-if-changed=../../cpp/src/ringbuf.cpp");
}
