#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

fn main() {
    #[cfg(target_os = "windows")]
    shader_compilation::compile_shaders();
}

#[cfg(target_os = "windows")]
mod shader_compilation {
    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
        process::{self, Command},
    };

    const SHADER_SOURCES: &[&str] = &[
        "src/shaders.hlsl",
        "src/color_text_raster.hlsl",
        "src/alpha_correction.hlsl",
    ];

    const SHADER_MODULES: &[ShaderModuleSpec] = &[
        ShaderModuleSpec::new("quad", "src/shaders.hlsl"),
        ShaderModuleSpec::new("shadow", "src/shaders.hlsl"),
        ShaderModuleSpec::new("path_rasterization", "src/shaders.hlsl"),
        ShaderModuleSpec::new("path_sprite", "src/shaders.hlsl"),
        ShaderModuleSpec::new("underline", "src/shaders.hlsl"),
        ShaderModuleSpec::new("monochrome_sprite", "src/shaders.hlsl"),
        ShaderModuleSpec::new("subpixel_sprite", "src/shaders.hlsl"),
        ShaderModuleSpec::new("polychrome_sprite", "src/shaders.hlsl"),
        ShaderModuleSpec::new("emoji_rasterization", "src/color_text_raster.hlsl"),
    ];

    #[derive(Copy, Clone)]
    struct ShaderModuleSpec {
        module: &'static str,
        shader_path: &'static str,
    }

    impl ShaderModuleSpec {
        const fn new(module: &'static str, shader_path: &'static str) -> Self {
            Self {
                module,
                shader_path,
            }
        }
    }

    #[derive(Copy, Clone)]
    enum ShaderCompilerKind {
        Fxc,
        Dxc,
    }

    impl ShaderCompilerKind {
        fn display_name(self) -> &'static str {
            match self {
                Self::Fxc => "FXC",
                Self::Dxc => "DXC",
            }
        }

        fn binary_name(self) -> &'static str {
            match self {
                Self::Fxc => "fxc.exe",
                Self::Dxc => "dxc.exe",
            }
        }

        fn env_var(self) -> &'static str {
            match self {
                Self::Fxc => "GPUI_FXC_PATH",
                Self::Dxc => "GPUI_DXC_PATH",
            }
        }

        fn rust_binding_name(self) -> &'static str {
            match self {
                Self::Fxc => "shaders_fxc_bytes.rs",
                Self::Dxc => "shaders_dxc_bytes.rs",
            }
        }

        fn artifact_prefix(self) -> &'static str {
            match self {
                Self::Fxc => "fxc",
                Self::Dxc => "dxc",
            }
        }

        fn shader_profile(self, target: ShaderTarget) -> &'static str {
            match (self, target) {
                (Self::Fxc, ShaderTarget::Vertex) => "vs_4_1",
                (Self::Fxc, ShaderTarget::Fragment) => "ps_4_1",
                (Self::Dxc, ShaderTarget::Vertex) => "vs_6_0",
                (Self::Dxc, ShaderTarget::Fragment) => "ps_6_0",
            }
        }

        fn add_arguments(
            self,
            command: &mut Command,
            entry_point: &str,
            output_path: &Path,
            var_name: &str,
            shader_path: &Path,
            target: ShaderTarget,
        ) {
            match self {
                Self::Fxc => {
                    command.args([
                        "/T",
                        self.shader_profile(target),
                        "/E",
                        entry_point,
                        "/Fh",
                        output_path.to_str().unwrap(),
                        "/Vn",
                        var_name,
                        "/O3",
                        shader_path.to_str().unwrap(),
                    ]);
                }
                Self::Dxc => {
                    command.args([
                        "-T",
                        self.shader_profile(target),
                        "-E",
                        entry_point,
                        "-Fh",
                        output_path.to_str().unwrap(),
                        "-Vn",
                        var_name,
                        "-O3",
                        shader_path.to_str().unwrap(),
                    ]);
                }
            }
        }
    }

    #[derive(Copy, Clone)]
    enum ShaderTarget {
        Vertex,
        Fragment,
    }

    impl ShaderTarget {
        fn entry_suffix(self) -> &'static str {
            match self {
                Self::Vertex => "vertex",
                Self::Fragment => "fragment",
            }
        }

        fn file_suffix(self) -> &'static str {
            match self {
                Self::Vertex => "vs",
                Self::Fragment => "ps",
            }
        }

        fn const_suffix(self) -> &'static str {
            match self {
                Self::Vertex => "VERTEX",
                Self::Fragment => "FRAGMENT",
            }
        }
    }

    pub fn compile_shaders() {
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-env-changed=GPUI_FXC_PATH");
        println!("cargo:rerun-if-env-changed=GPUI_DXC_PATH");

        for shader_source in SHADER_SOURCES {
            println!("cargo:rerun-if-changed={shader_source}");
        }

        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let fxc_path = find_shader_compiler(ShaderCompilerKind::Fxc);
        let dxc_path = find_shader_compiler(ShaderCompilerKind::Dxc);

        compile_shader_family(
            ShaderCompilerKind::Fxc,
            &fxc_path,
            &manifest_dir,
            &out_dir.join(ShaderCompilerKind::Fxc.rust_binding_name()),
        );
        compile_shader_family(
            ShaderCompilerKind::Dxc,
            &dxc_path,
            &manifest_dir,
            &out_dir.join(ShaderCompilerKind::Dxc.rust_binding_name()),
        );
    }

    fn compile_shader_family(
        compiler_kind: ShaderCompilerKind,
        compiler_path: &Path,
        manifest_dir: &Path,
        rust_binding_path: &Path,
    ) {
        if rust_binding_path.exists() {
            fs::remove_file(rust_binding_path).expect("Failed to remove existing shader binding");
        }

        for module in SHADER_MODULES {
            compile_shader_for_module(
                compiler_kind,
                compiler_path,
                manifest_dir,
                rust_binding_path,
                *module,
            );
        }
    }

    fn compile_shader_for_module(
        compiler_kind: ShaderCompilerKind,
        compiler_path: &Path,
        manifest_dir: &Path,
        rust_binding_path: &Path,
        module: ShaderModuleSpec,
    ) {
        let shader_path = manifest_dir.join(module.shader_path);

        for target in [ShaderTarget::Vertex, ShaderTarget::Fragment] {
            let entry_point = format!("{}_{}", module.module, target.entry_suffix());
            let const_name = format!(
                "{}_{}_BYTES",
                module.module.to_uppercase(),
                target.const_suffix()
            );
            let output_path = rust_binding_path.with_file_name(format!(
                "{}_{}_{}.h",
                compiler_kind.artifact_prefix(),
                module.module,
                target.file_suffix()
            ));

            compile_shader_impl(
                compiler_kind,
                compiler_path,
                &entry_point,
                &output_path,
                &const_name,
                &shader_path,
                target,
            );
            generate_rust_binding(&const_name, &output_path, rust_binding_path);
        }
    }

    fn compile_shader_impl(
        compiler_kind: ShaderCompilerKind,
        compiler_path: &Path,
        entry_point: &str,
        output_path: &Path,
        var_name: &str,
        shader_path: &Path,
        target: ShaderTarget,
    ) {
        let mut command = Command::new(compiler_path);
        compiler_kind.add_arguments(
            &mut command,
            entry_point,
            output_path,
            var_name,
            shader_path,
            target,
        );

        match command.output() {
            Ok(result) if result.status.success() => {}
            Ok(result) => {
                println!(
                    "cargo::error={} shader compilation failed for {}:\nstdout:\n{}\nstderr:\n{}",
                    compiler_kind.display_name(),
                    entry_point,
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr),
                );
                process::exit(1);
            }
            Err(error) => {
                println!(
                    "cargo::error=Failed to run {} for {}: {}",
                    compiler_kind.display_name(),
                    entry_point,
                    error,
                );
                process::exit(1);
            }
        }
    }

    fn generate_rust_binding(const_name: &str, header_path: &Path, output_path: &Path) {
        let header_content = fs::read_to_string(header_path).expect("Failed to read shader header");
        let const_definition = extract_const_definition(&header_content, const_name);
        let rust_binding = format!(
            "pub(super) const {}: &[u8] = &{};\n",
            const_name,
            const_definition.replace('{', "[").replace('}', "]")
        );

        let mut output = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_path)
            .expect("Failed to open Rust shader binding");
        output
            .write_all(rust_binding.as_bytes())
            .expect("Failed to write Rust shader binding");
    }

    fn extract_const_definition(header_content: &str, const_name: &str) -> String {
        let needle = format!("{const_name}[");
        let const_name_offset = header_content
            .find(&needle)
            .or_else(|| header_content.find(const_name))
            .expect("Failed to locate shader bytes in generated header");
        let declaration_start = header_content[..const_name_offset]
            .rfind("const ")
            .expect("Failed to locate shader declaration");
        let declaration = &header_content[declaration_start..];
        let equals_offset = declaration
            .find('=')
            .expect("Failed to locate shader declaration assignment");
        let semicolon_offset = declaration
            .find(';')
            .expect("Failed to locate shader declaration terminator");

        declaration[equals_offset + 1..semicolon_offset]
            .trim()
            .to_string()
    }

    /// Locate `binary` in the newest installed Windows SDK.
    fn find_latest_windows_sdk_binary(
        binary: &str,
    ) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
        let key = windows_registry::LOCAL_MACHINE
            .open("SOFTWARE\\WOW6432Node\\Microsoft\\Microsoft SDKs\\Windows\\v10.0")?;

        let install_folder: String = key.get_string("InstallationFolder")?;
        let install_folder_bin = Path::new(&install_folder).join("bin");

        let mut versions: Vec<_> = std::fs::read_dir(&install_folder_bin)?
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();

        versions.sort_by_key(|version| {
            version
                .split('.')
                .filter_map(|part| part.parse().ok())
                .collect::<Vec<u32>>()
        });

        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            arch => Err(format!("Unsupported architecture: {arch}"))?,
        };

        Ok(versions.last().map(|highest_version| {
            install_folder_bin
                .join(highest_version)
                .join(arch)
                .join(binary)
        }))
    }

    fn find_shader_compiler(compiler_kind: ShaderCompilerKind) -> PathBuf {
        if let Ok(path) = std::env::var(compiler_kind.env_var()) {
            let path = PathBuf::from(path);
            if path.exists() {
                return path;
            }
        }

        if let Ok(output) = Command::new("where.exe")
            .arg(compiler_kind.binary_name())
            .output()
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let first_path = stdout
                .lines()
                .next()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .expect("where.exe returned an empty path");
            return PathBuf::from(first_path);
        }

        if let Ok(Some(path)) = find_latest_windows_sdk_binary(compiler_kind.binary_name()) {
            return path;
        }

        panic!(
            "Failed to find {}. Set {} to an explicit compiler path.",
            compiler_kind.binary_name(),
            compiler_kind.env_var(),
        );
    }
}
