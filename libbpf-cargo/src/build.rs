use std::collections::HashSet;
use std::env::consts::ARCH;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use regex::Regex;
use semver::Version;
use tempfile::tempdir;

use crate::metadata;
use crate::metadata::UnprocessedObj;

fn check_progs(objs: &[UnprocessedObj]) -> Result<()> {
    let mut set = HashSet::with_capacity(objs.len());
    for obj in objs {
        // OK to unwrap() file_name() b/c we already checked earlier that this is a valid file
        let dest = obj
            .out
            .as_path()
            .join(obj.path.as_path().file_name().unwrap());
        if !set.insert(dest) {
            bail!(
                "Duplicate obj={} detected",
                obj.path.as_path().file_name().unwrap().to_string_lossy()
            );
        }
    }

    Ok(())
}

fn extract_version(output: &str) -> Result<&str> {
    let re = Regex::new(r"clang\s+version\s+(?P<version_str>\d+\.\d+\.\d+)")?;
    let captures = re
        .captures(output)
        .ok_or_else(|| anyhow!("Failed to run regex on version string"))?;

    captures.name("version_str").map_or_else(
        || Err(anyhow!("Failed to find version capture group")),
        |v| Ok(v.as_str()),
    )
}

/// Extract vendored libbpf header files to a temporary directory.
///
/// Directory and enclosed contents will be removed when return object is dropped.
#[cfg(not(feature = "novendor"))]
fn extract_libbpf_headers_to_disk(target_dir: &Path) -> Result<Option<PathBuf>> {
    use std::fs::OpenOptions;
    use std::io::Write;

    let parent_dir = target_dir.join("bpf").join("src");
    let dir = parent_dir.join("bpf");
    fs::create_dir_all(&dir)?;
    for (filename, contents) in libbpf_sys::API_HEADERS.iter() {
        let path = dir.as_path().join(filename);
        let mut file = OpenOptions::new().write(true).create(true).open(path)?;
        file.write_all(contents.as_bytes())?;
    }

    Ok(Some(parent_dir))
}

#[cfg(feature = "novendor")]
fn extract_libbpf_headers_to_disk(target_dir: &Path) -> Result<Option<PathBuf>> {
    return Ok(None);
}

fn check_clang(debug: bool, clang: &Path, skip_version_checks: bool) -> Result<()> {
    let output = Command::new(clang.as_os_str())
        .arg("--version")
        .output()
        .context("Failed to execute clang")?;

    if !output.status.success() {
        bail!("Failed to execute clang binary");
    }

    if skip_version_checks {
        return Ok(());
    }

    // Example output:
    //
    //     clang version 10.0.0
    //     Target: x86_64-pc-linux-gnu
    //     Thread model: posix
    //     InstalledDir: /bin
    //
    let output = String::from_utf8_lossy(&output.stdout);
    let version_str = extract_version(&output)?;
    let version = Version::parse(version_str)?;
    if debug {
        println!("{} is version {}", clang.display(), version);
    }

    if version < Version::parse("10.0.0").unwrap() {
        bail!(
            "version {} is too old. Use --skip-clang-version-checks to skip version check",
            version
        );
    }

    Ok(())
}

/// Strip DWARF information from the provided BPF object file.
///
/// We rely on the `libbpf` linker here, which removes debug information as a
/// side-effect.
fn strip_dwarf_info(file: &Path) -> Result<()> {
    let mut temp_file = file.as_os_str().to_os_string();
    temp_file.push(".tmp");

    fs::rename(file, &temp_file).context("Failed to rename compiled BPF object file")?;

    let mut linker =
        libbpf_rs::Linker::new(file).context("Failed to instantiate libbpf object file linker")?;
    linker
        .add_file(temp_file)
        .context("Failed to add object file to BPF linker")?;
    linker.link().context("Failed to link object file")?;
    Ok(())
}

/// Concatenate a command and its arguments into a single string.
fn concat_command<C, A, S>(command: C, args: A) -> OsString
where
    C: AsRef<OsStr>,
    A: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .fold(command.as_ref().to_os_string(), |mut cmd, arg| {
            cmd.push(OsStr::new(" "));
            cmd.push(arg.as_ref());
            cmd
        })
}

/// Format a command with the given list of arguments as a string.
fn format_command(command: &Command) -> String {
    let prog = command.get_program();
    let args = command.get_args();

    concat_command(prog, args).to_string_lossy().to_string()
}

/// We're essentially going to run:
///
///   clang -g -O2 -target bpf -c -D__TARGET_ARCH_$(ARCH) runqslower.bpf.c -o runqslower.bpf.o
///
/// for each prog.
fn compile_one(debug: bool, source: &Path, out: &Path, clang: &Path, options: &str) -> Result<()> {
    if debug {
        println!("Building {}", source.display());
    }

    let mut cmd = Command::new(clang.as_os_str());

    if !options.is_empty() {
        cmd.args(options.split_whitespace());
    }

    if !options.contains("-D__TARGET_ARCH_") {
        let arch = match ARCH {
            "x86_64" => "x86",
            "aarch64" => "arm64",
            "powerpc64" => "powerpc",
            "s390x" => "s390",
            _ => ARCH,
        };
        cmd.arg(format!("-D__TARGET_ARCH_{arch}"));
    }

    cmd.arg("-g")
        .arg("-O2")
        .arg("-target")
        .arg("bpf")
        .arg("-c")
        .arg(source.as_os_str())
        .arg("-o")
        .arg(out);

    let output = cmd.output().context("Failed to execute clang")?;
    if !output.status.success() {
        let err = Err(anyhow!(String::from_utf8_lossy(&output.stderr).to_string()))
            .with_context(|| {
                format!(
                    "Command `{}` failed ({})",
                    format_command(&cmd),
                    output.status
                )
            })
            .with_context(|| {
                format!(
                    "Failed to compile {} from {}",
                    out.display(),
                    source.display()
                )
            });
        return err;
    }

    // Compilation with clang may contain DWARF information that references
    // system specific and temporary paths. That can render our generated
    // skeletons unstable, potentially rendering them unsuitable for inclusion
    // in version control systems. So strip this information.
    strip_dwarf_info(out).with_context(|| format!("Failed to strip object file {}", out.display()))
}

fn compile(debug: bool, objs: &[UnprocessedObj], clang: &Path, target_dir: &Path) -> Result<()> {
    let header_dir = extract_libbpf_headers_to_disk(target_dir)?;
    let compiler_options = if let Some(dir) = &header_dir {
        format!("-I{}", dir.to_str().unwrap())
    } else {
        "".to_string()
    };

    for obj in objs {
        let stem = obj.path.file_stem().with_context(|| {
            format!(
                "Could not calculate destination name for obj={}",
                obj.path.display()
            )
        })?;

        let mut dest_name = stem.to_os_string();
        dest_name.push(".o");

        let mut dest_path = obj.out.to_path_buf();
        dest_path.push(&dest_name);
        fs::create_dir_all(&obj.out)?;
        compile_one(debug, &obj.path, &dest_path, clang, &compiler_options)?;
    }

    Ok(())
}

fn extract_clang_or_default(clang: Option<&PathBuf>) -> PathBuf {
    match clang {
        Some(c) => c.into(),
        // Searches $PATH
        None => "clang".into(),
    }
}

pub fn build(
    debug: bool,
    manifest_path: Option<&PathBuf>,
    clang: Option<&PathBuf>,
    skip_clang_version_checks: bool,
) -> Result<()> {
    let (target_dir, to_compile) = metadata::get(debug, manifest_path)?;

    if debug && !to_compile.is_empty() {
        println!("Found bpf progs to compile:");
        for obj in &to_compile {
            println!("\t{obj:?}");
        }
    } else if to_compile.is_empty() {
        bail!("Did not find any bpf progs to compile");
    }

    check_progs(&to_compile)?;

    let clang = extract_clang_or_default(clang);
    check_clang(debug, &clang, skip_clang_version_checks)
        .with_context(|| anyhow!("{} is invalid", clang.display()))?;
    compile(debug, &to_compile, &clang, &target_dir).context("Failed to compile progs")?;

    Ok(())
}

// Only used in libbpf-cargo library
#[allow(dead_code)]
pub fn build_single(
    debug: bool,
    source: &Path,
    out: &Path,
    clang: Option<&PathBuf>,
    skip_clang_version_checks: bool,
    options: &str,
) -> Result<()> {
    let clang = extract_clang_or_default(clang);
    check_clang(debug, &clang, skip_clang_version_checks)?;
    let header_parent_dir = tempdir()?;
    let header_dir = extract_libbpf_headers_to_disk(header_parent_dir.path())?;
    let mut compiler_options = if let Some(dir) = &header_dir {
        format!("{} -I{}", options, dir.to_str().unwrap())
    } else {
        options.to_string()
    };

    // Explicitly disable stack protector logic, which doesn't work with
    // BPF. See https://lkml.org/lkml/2020/2/21/1000.
    compiler_options += " -fno-stack-protector";

    compile_one(debug, source, out, &clang, &compiler_options)?;

    Ok(())
}

#[test]
fn test_extract_version() {
    let upstream_format = r"clang version 10.0.0
Target: x86_64-pc-linux-gnu
Thread model: posix
InstalledDir: /bin
";
    assert_eq!(extract_version(upstream_format).unwrap(), "10.0.0");

    let ubuntu_format = r"Ubuntu clang version 11.0.1-++20201121072624+973b95e0a84-1~exp1~20201121063303.19
Target: x86_64-pc-linux-gnu
Thread model: posix
InstalledDir: /bin
";
    assert_eq!(extract_version(ubuntu_format).unwrap(), "11.0.1");

    assert!(extract_version("askldfjwe").is_err());
    assert!(extract_version("my clang version 1.5").is_err());
}
