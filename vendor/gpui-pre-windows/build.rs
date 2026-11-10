#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

//! Shader bytecode comes from one of two places.
//!
//! * Default: the app compiles the embedded HLSL when it starts, with the system
//!   `d3dcompiler_47.dll`. This is what a cross-build from Linux gets, since there is no `fxc.exe`
//!   there to precompile with.
//! * Feature `precompiled-shaders`: this script compiles every shader with `fxc.exe` from the
//!   Windows SDK and the bytecode is built into the executable. It needs a Windows host.
//!
//! Setting `DH_REQUIRE_PRECOMPILED_SHADERS` makes a build without the feature fail, so a release
//! cannot be made with run-time compilation by accident.

mod shader_header;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DH_REQUIRE_PRECOMPILED_SHADERS");
    println!("cargo:rerun-if-env-changed=GPUI_FXC_PATH");

    let precompiled = std::env::var_os("CARGO_FEATURE_PRECOMPILED_SHADERS").is_some();
    let required = std::env::var_os("DH_REQUIRE_PRECOMPILED_SHADERS")
        .is_some_and(|v| !v.is_empty() && v != "0");
    let target_windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    if !target_windows {
        return;
    }
    if required && !precompiled {
        println!(
            "cargo::error=DH_REQUIRE_PRECOMPILED_SHADERS is set but the precompiled-shaders feature is off, \
             so this build would compile shaders when the app starts"
        );
        std::process::exit(1);
    }
    if !precompiled {
        return;
    }
    #[cfg(target_os = "windows")]
    shader_compilation::compile_shaders();
    #[cfg(not(target_os = "windows"))]
    {
        println!(
            "cargo::error=precompiled-shaders needs a Windows host, because it runs fxc.exe from the Windows SDK"
        );
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
mod shader_compilation {
    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
        process::{self, Command},
    };

    /// Every blob generated so far, to count the distinct ones at the end.
    static BLOBS: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());

    pub fn compile_shaders() {
        let shader_path =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/shaders.hlsl");
        let out_dir = std::env::var("OUT_DIR").unwrap();

        println!("cargo:rerun-if-changed={}", shader_path.display());

        // Check if fxc.exe is available
        let fxc_path = find_fxc_compiler();

        // Define all modules
        let modules = [
            "quad",
            "shadow",
            "path_rasterization",
            "path_sprite",
            "underline",
            "monochrome_sprite",
            "subpixel_sprite",
            "polychrome_sprite",
        ];

        let rust_binding_path = format!("{}/shaders_bytes.rs", out_dir);
        if Path::new(&rust_binding_path).exists() {
            fs::remove_file(&rust_binding_path)
                .expect("Failed to remove existing Rust binding file");
        }
        for module in modules {
            compile_shader_for_module(
                module,
                &out_dir,
                &fxc_path,
                shader_path.to_str().unwrap(),
                &rust_binding_path,
            );
        }

        {
            let shader_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
                .join("src/color_text_raster.hlsl");
            compile_shader_for_module(
                "emoji_rasterization",
                &out_dir,
                &fxc_path,
                shader_path.to_str().unwrap(),
                &rust_binding_path,
            );
        }

        // Entry points whose bodies are identical (`subpixel_sprite_vertex` only calls
        // `monochrome_sprite_vertex`) give identical bytecode, which the linker folds into one
        // constant. `DH_SHADER_REPORT` names a file that receives the distinct count, for
        // `scripts/check-release-binary.sh` to compare against the executable.
        println!("cargo:rerun-if-env-changed=DH_SHADER_REPORT");
        if let Some(report) = std::env::var_os("DH_SHADER_REPORT").filter(|v| !v.is_empty()) {
            let mut blobs = BLOBS.lock().unwrap().clone();
            blobs.sort();
            blobs.dedup();
            fs::write(report, format!("{}\n", blobs.len())).expect("Failed to write shader report");
        }
    }

    /// Locate `binary` in the newest installed Windows SDK.
    pub fn find_latest_windows_sdk_binary(
        binary: &str,
    ) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
        let key = windows_registry::LOCAL_MACHINE
            .open("SOFTWARE\\WOW6432Node\\Microsoft\\Microsoft SDKs\\Windows\\v10.0")?;

        let install_folder: String = key.get_string("InstallationFolder")?; // "C:\Program Files (x86)\Windows Kits\10\"
        let install_folder_bin = Path::new(&install_folder).join("bin");

        let mut versions: Vec<_> = std::fs::read_dir(&install_folder_bin)?
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();

        versions.sort_by_key(|s| {
            s.split('.')
                .filter_map(|p| p.parse().ok())
                .collect::<Vec<u32>>()
        });

        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            _ => Err(format!(
                "Unsupported architecture: {}",
                std::env::consts::ARCH
            ))?,
        };

        // Newest first, and only a version that has the tool: some SDK folders are partial.
        for version in versions.iter().rev() {
            let candidate = install_folder_bin.join(version).join(arch).join(binary);
            if candidate.exists() {
                return Ok(Some(candidate));
            }
        }

        Ok(None)
    }

    /// You can set the `GPUI_FXC_PATH` environment variable to specify the path to the fxc.exe compiler.
    fn find_fxc_compiler() -> String {
        // Check environment variable
        if let Ok(path) = std::env::var("GPUI_FXC_PATH")
            && Path::new(&path).exists()
        {
            return path;
        }

        // Try to find in PATH
        // NOTE: This has to be `where.exe` on Windows, not `where`, it must be ended with `.exe`
        if let Ok(output) = std::process::Command::new("where.exe")
            .arg("fxc.exe")
            .output()
            && output.status.success()
        {
            // `where.exe` prints one line per match; take the first.
            let path = String::from_utf8_lossy(&output.stdout);
            if let Some(first) = path.lines().map(str::trim).find(|l| !l.is_empty()) {
                return first.to_string();
            }
        }

        if let Ok(Some(path)) = find_latest_windows_sdk_binary("fxc.exe") {
            return path.to_string_lossy().into_owned();
        }

        panic!("Failed to find fxc.exe");
    }

    fn compile_shader_for_module(
        module: &str,
        out_dir: &str,
        fxc_path: &str,
        shader_path: &str,
        rust_binding_path: &str,
    ) {
        // Compile vertex shader
        let output_file = format!("{}/{}_vs.h", out_dir, module);
        let const_name = format!("{}_VERTEX_BYTES", module.to_uppercase());
        compile_shader_impl(
            fxc_path,
            &format!("{module}_vertex"),
            &output_file,
            &const_name,
            shader_path,
            "vs_4_1",
        );
        generate_rust_binding(&const_name, &output_file, rust_binding_path);

        // Compile fragment shader
        let output_file = format!("{}/{}_ps.h", out_dir, module);
        let const_name = format!("{}_FRAGMENT_BYTES", module.to_uppercase());
        compile_shader_impl(
            fxc_path,
            &format!("{module}_fragment"),
            &output_file,
            &const_name,
            shader_path,
            "ps_4_1",
        );
        generate_rust_binding(&const_name, &output_file, rust_binding_path);
    }

    fn compile_shader_impl(
        fxc_path: &str,
        entry_point: &str,
        output_path: &str,
        var_name: &str,
        shader_path: &str,
        target: &str,
    ) {
        let output = Command::new(fxc_path)
            .args([
                "/T",
                target,
                "/E",
                entry_point,
                "/Fh",
                output_path,
                "/Vn",
                var_name,
                "/O3",
                shader_path,
            ])
            .output();

        match output {
            Ok(result) => {
                if result.status.success() {
                    return;
                }
                println!(
                    "cargo::error=Shader compilation failed for {}:\n{}",
                    entry_point,
                    String::from_utf8_lossy(&result.stderr)
                );
                process::exit(1);
            }
            Err(e) => {
                println!("cargo::error=Failed to run fxc for {}: {}", entry_point, e);
                process::exit(1);
            }
        }
    }

    fn generate_rust_binding(const_name: &str, head_file: &str, output_path: &str) {
        let header_content = fs::read_to_string(head_file).expect("Failed to read header file");
        let global_var_start = header_content.find("const BYTE").unwrap();
        let global_var = &header_content[global_var_start..];
        let equal = global_var.find('=').unwrap();
        let const_definition = global_var[equal + 1..].trim();
        let bytes = crate::shader_header::parse_fxc_header(&header_content).unwrap_or_default();
        if !crate::shader_header::is_dxbc(&bytes) {
            println!(
                "cargo::error=Shader {const_name} compiled to {} bytes that are not a DXBC blob",
                bytes.len()
            );
            process::exit(1);
        }
        BLOBS.lock().unwrap().push(bytes);
        let rust_binding = format!(
            "const {}: &[u8] = &{}\n",
            const_name,
            const_definition.replace('{', "[").replace('}', "]")
        );
        let mut options = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_path)
            .expect("Failed to open Rust binding file");
        options
            .write_all(rust_binding.as_bytes())
            .expect("Failed to write Rust binding file");
    }
}
