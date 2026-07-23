use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use base64::{Engine, alphabet, engine::GeneralPurpose, engine::GeneralPurposeConfig};
use regex::Regex;
use sha1::{Digest, Sha1};

const NID_SALT: &str = "518D64A635DED8C1E6B039B1C3E55230";

struct SyscallEntry {
    id: u64,
    name: String,
    nid: String,
}

struct MetaEntry {
    name: String,
    args: Vec<(String, String)>, // (Type, Name)
}

fn main() -> anyhow::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR")?;
    let root = PathBuf::from(manifest_dir);

    let workspace_root = root.parent().unwrap().parent().unwrap();

    let fixed_path = workspace_root.join("data/wiki_syscalls.txt");
    let dynamic_path = workspace_root.join("data/ps4_names.txt");
    let sdk_root = workspace_root.join("data/oo_sdk");
    let sdk_path = sdk_root.join("include");

    println!("cargo:rerun-if-changed={}", fixed_path.display());
    println!("cargo:rerun-if-changed={}", dynamic_path.display());

    if !Path::new(&sdk_root).exists() {
        // cargo:warning renders bold/yellow in the terminal
        println!("cargo:warning=⚠️  ========================================================");
        println!(
            "cargo:warning=⚠️  OpenOrbis SDK NOT FOUND at '{}'",
            sdk_root.display()
        );
        println!(
            "cargo:warning=⚠️  Syscall arguments metadata will be MISSING (empty signatures)."
        );
        println!("cargo:warning=⚠️  To fix this, run the following command in the project root:");
        println!("cargo:warning=");
        println!(
            "cargo:warning=    git clone https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain.git data/oo_sdk"
        );
        println!("cargo:warning=");
        println!("cargo:warning=⚠️  ========================================================");
    } else {
        println!("cargo:rerun-if-changed={}", sdk_path.display());
    }

    let syscalls = load_syscalls(
        &fixed_path.display().to_string(),
        &dynamic_path.display().to_string(),
    )?;
    generate_syscalls_rs(&syscalls)?;

    let known_names: HashSet<String> = syscalls.iter().map(|s| s.name.clone()).collect();
    let metadata = load_metadata(&sdk_path.display().to_string(), &known_names)?;
    generate_metadata_rs(&metadata)?;

    Ok(())
}

// syscall id tables

fn load_syscalls(fixed_path: &str, dynamic_path: &str) -> anyhow::Result<Vec<SyscallEntry>> {
    let mut entries = Vec::new();
    let mut seen_names = HashSet::new();

    // fixed ids from the wiki list
    if Path::new(fixed_path).exists() {
        let file = File::open(fixed_path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2
                && let Ok(id) = parts[0].parse::<u64>()
            {
                let name = parts[1].to_string();
                if !seen_names.contains(&name) {
                    let nid = calculate_nid(&name);
                    seen_names.insert(name.clone());
                    entries.push(SyscallEntry { id, name, nid });
                }
            }
        }
    }

    // dynamic names get synthetic ids starting at 10000
    let mut custom_id = 10000;
    if Path::new(dynamic_path).exists() {
        let file = File::open(dynamic_path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }
            let mut name = parts[0].to_string();
            if parts.len() > 1 {
                if parts[0].chars().all(char::is_numeric) {
                    name = parts[1].to_string();
                } else if parts[1].chars().all(char::is_numeric) {
                    name = parts[0].to_string();
                }
            }
            if !seen_names.contains(&name) {
                let nid = calculate_nid(&name);
                seen_names.insert(name.clone());
                entries.push(SyscallEntry {
                    id: custom_id,
                    name,
                    nid,
                });
                custom_id += 1;
            }
        }
    }
    entries.sort_by_key(|e| e.id);
    Ok(entries)
}

fn generate_syscalls_rs(entries: &[SyscallEntry]) -> anyhow::Result<()> {
    let out_dir = env::var("OUT_DIR")?;
    let dest_path = Path::new(&out_dir).join("generated_syscalls.rs");
    let mut f = File::create(&dest_path)?;

    writeln!(f, "// --- AUTO-GENERATED BY build.rs (Syscalls) ---")?;

    // sorted copies for the lookup tables
    let mut by_name_entries: Vec<&SyscallEntry> = entries.iter().collect();
    by_name_entries.sort_by(|a, b| a.name.cmp(&b.name));

    let mut by_nid_entries: Vec<&SyscallEntry> = entries.iter().collect();
    by_nid_entries.sort_by(|a, b| a.nid.cmp(&b.nid));

    // per-id constants
    let mut used_names = HashSet::new();
    writeln!(f, "impl SyscallId {{")?;
    for entry in entries {
        let const_name = to_const_name(&entry.name, entry.id, &mut used_names);
        writeln!(
            f,
            "    pub const {}: Self = Self({});",
            const_name, entry.id
        )?;
    }
    writeln!(f, "}}")?;

    // id -> nid, sorted by id for binary search (entries are already sorted by
    // id above). Emitting a rodata slice keeps ps4-syscalls compiling fast --
    // a 94k-arm match here dominated the crate's codegen time (TASK-19).
    writeln!(f, "\nstatic MAP_ID_TO_NID: &[(u64, &str)] = &[")?;
    for e in entries {
        writeln!(f, "    ({}, \"{}\"),", e.id, e.nid)?;
    }
    writeln!(f, "];")?;

    writeln!(f, "\nstatic MAP_BY_ID: &[(u64, &str)] = &[")?;
    for e in entries {
        writeln!(f, "    ({}, \"{}\"),", e.id, e.name)?;
    }
    writeln!(f, "];")?;

    writeln!(f, "\nstatic MAP_BY_NAME: &[(&str, u64)] = &[")?;
    for e in by_name_entries {
        writeln!(f, "    (\"{}\", {}),", e.name, e.id)?;
    }
    writeln!(f, "];")?;

    writeln!(f, "\nstatic MAP_BY_NID: &[(&str, u64)] = &[")?;
    for e in by_nid_entries {
        writeln!(f, "    (\"{}\", {}),", e.nid, e.id)?;
    }
    writeln!(f, "];")?;

    Ok(())
}

// arg metadata scraped from sdk headers

fn load_metadata(sdk_path: &str, known_names: &HashSet<String>) -> anyhow::Result<Vec<MetaEntry>> {
    let mut meta_map = HashMap::new();
    let root_path = Path::new(sdk_path);

    if !root_path.exists() {
        println!(
            "cargo:warning=SDK path {} not found. Metadata will be empty.",
            sdk_path
        );
        return Ok(Vec::new());
    }

    // matches C function prototypes: return-type name(args);
    let re = Regex::new(r"([a-zA-Z0-9_ \*]+)\s+([a-zA-Z0-9_]+)\s*\(([^;)]*)\)\s*;")?;
    // collapses runs of whitespace; compiled once, reused per header file
    let space_re = Regex::new(r"\s+").unwrap();

    // walk the include tree with an explicit stack
    let mut dirs = vec![root_path.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "h")
                    && let Ok(content) = fs::read_to_string(&path)
                {
                    // flatten to a single line so the regex can match across newlines
                    let clean_content = content.replace(['\n', '\t'], " ");
                    let single_line = space_re.replace_all(&clean_content, " ");

                    for cap in re.captures_iter(&single_line) {
                        let name = cap[2].to_string();

                        // skip dupes and functions that aren't known syscalls
                        if meta_map.contains_key(&name) || !known_names.contains(&name) {
                            continue;
                        }

                        let args_str = cap[3].trim();
                        let mut args = Vec::new();

                        if !args_str.is_empty() && args_str != "void" {
                            for raw_arg in args_str.split(',') {
                                let raw_arg = raw_arg.trim();
                                if raw_arg.is_empty() {
                                    continue;
                                }

                                let cleaned =
                                    raw_arg.replace("const ", "").replace("restrict ", "");
                                let cleaned = cleaned.trim();

                                // last space splits type from name
                                if let Some((t, n)) = cleaned.rsplit_once(' ') {
                                    args.push((clean_string(t), clean_string(n)));
                                } else {
                                    // unnamed arg, e.g. "void*"
                                    args.push((clean_string(cleaned), "?".to_string()));
                                }
                            }
                        }

                        meta_map.insert(name.clone(), MetaEntry { name, args });
                    }
                }
            }
        }
    }

    let mut result: Vec<MetaEntry> = meta_map.into_values().collect();
    // sorted by name for binary search at runtime
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

fn generate_metadata_rs(entries: &[MetaEntry]) -> anyhow::Result<()> {
    let out_dir = env::var("OUT_DIR")?;
    let dest_path = Path::new(&out_dir).join("generated_metadata.rs");
    let mut f = File::create(&dest_path)?;

    writeln!(f, "// --- AUTO-GENERATED BY build.rs (Metadata) ---")?;
    writeln!(f, "// Sorted by name for binary search\n")?;

    writeln!(f, "static METADATA_TABLE: &[SyscallMeta] = &[")?;

    for entry in entries {
        writeln!(f, "    SyscallMeta {{")?;
        writeln!(f, "        name: \"{}\",", entry.name)?;
        writeln!(f, "        arg_count: {},", entry.args.len())?;
        writeln!(f, "        args: &[")?;
        for (t, n) in &entry.args {
            writeln!(f, "            (\"{}\", \"{}\"),", t, n)?;
        }
        writeln!(f, "        ],")?;
        writeln!(f, "    }},")?;
    }

    writeln!(f, "];")?;

    Ok(())
}

fn clean_string(s: &str) -> String {
    s.replace(['"', '\'', '\\'], "").trim().to_string()
}

fn calculate_nid(name: &str) -> String {
    let salt_bytes = hex::decode(NID_SALT).expect("Invalid Hex Salt");
    let mut hasher = Sha1::new();
    hasher.update(name.as_bytes());
    hasher.update(&salt_bytes);
    let digest = hasher.finalize();
    let val_bytes: [u8; 8] = digest[0..8].try_into().unwrap();
    let val = u64::from_le_bytes(val_bytes);
    let hex_val = format!("{:016x}", val);
    let final_bytes = hex::decode(hex_val).unwrap();
    let alphabet =
        alphabet::Alphabet::new("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-")
            .unwrap();
    let engine = GeneralPurpose::new(&alphabet, GeneralPurposeConfig::new());
    engine.encode(final_bytes).trim_end_matches('=').to_string()
}

fn to_const_name(name: &str, id: u64, used: &mut HashSet<String>) -> String {
    // camelCase -> snake_case
    let mut snake = String::new();
    let chars = name.chars();
    let mut prev_char: Option<char> = None;

    for c in chars {
        if let Some(prev) = prev_char
            && c.is_uppercase()
            && (prev.is_lowercase() || prev.is_numeric())
        {
            snake.push('_');
        }
        snake.push(c);
        prev_char = Some(c);
    }

    // uppercase, strip non-alnum, dedupe underscores
    let mut clean = snake.to_uppercase();
    clean = clean.replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
    while clean.contains("__") {
        clean = clean.replace("__", "_");
    }
    clean = clean.trim_matches('_').to_string();

    if clean.is_empty() {
        clean = "SYS_INVALID".to_string();
    }
    if clean
        .chars()
        .next()
        .map(|c| c.is_numeric())
        .unwrap_or(false)
    {
        clean = format!("SYS_{}", clean);
    }

    if !clean.starts_with("SYS_") && !clean.starts_with("SCE_") {
        clean = format!("SYS_{}", clean);
    }

    if used.contains(&clean) {
        clean = format!("{}_{}", clean, id);
    }
    used.insert(clean.clone());
    clean
}
