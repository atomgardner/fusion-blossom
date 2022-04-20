extern crate cc;
use std::env;
use std::path::Path;

fn main() {

    if Path::new("./blossomV/PerfectMatching.h").exists() {

        println!("cargo:rustc-cfg=feature=\"blossom_v\"");

        let target_os = env::var("CARGO_CFG_TARGET_OS");

        let mut build = cc::Build::new();

        build.cpp(true)
            .file("./blossomV/blossomV.cpp")
            .file("./blossomV/PMinterface.cpp")
            .file("./blossomV/PMduals.cpp")
            .file("./blossomV/PMexpand.cpp")
            .file("./blossomV/PMinit.cpp")
            .file("./blossomV/PMmain.cpp")
            .file("./blossomV/PMrepair.cpp")
            .file("./blossomV/PMshrink.cpp")
            .file("./blossomV/misc.cpp")
            .file("./blossomV/MinCost/MinCost.cpp");
    
        if target_os != Ok("macos".to_string()) {  // exclude from macOS
            build.cpp_link_stdlib("stdc++"); // use libstdc++
            build.flag("-Wno-unused-but-set-variable");  // this option is not available in clang
        }

        // ignore warnings from blossom library
        build.flag("-Wno-unused-parameter")
            .flag("-Wno-unused-variable")
            .flag("-Wno-reorder-ctor")
            .compile("blossomV");

        println!("cargo:rerun-if-changed=./blossomV/blossomV.cpp");
    
        if target_os != Ok("macos".to_string()) {  // exclude from macOS
            println!("cargo:rustc-link-lib=static=stdc++");  // have to add this to compile c++ (new, delete operators)
        }

        println!("cargo:rustc-link-lib=static=blossomV");
    }
}