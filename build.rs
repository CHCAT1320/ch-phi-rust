//! 构建脚本：把 `assets/` 下除 `charts/` 外的所有资源内嵌进二进制。
//!
//! 生成 `$OUT_DIR/embedded_assets.rs`，其中 `FILES` 为 `(相对 assets 的键, 字节)` 列表，
//! 每个条目用 `include_bytes!` 在编译期读入。键使用正斜杠（如 `notes/Tap2.png`）。
//! 谱面、音乐、插画位于 `assets/charts/`，不内嵌，运行时从磁盘读取。

use std::fs;
use std::path::Path;

/// 递归收集目录下所有文件（跳过 `charts`），返回 `(键, 绝对路径)`。
fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = match path.strip_prefix(base) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        // 谱面目录（含音乐与插画）不内嵌。
        if rel == "charts" || rel.starts_with("charts/") {
            continue;
        }
        if path.is_dir() {
            walk(&path, base, out);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
            out.push((rel, path.to_string_lossy().replace('\\', "/")));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let assets = Path::new(&manifest).join("assets");
    println!("cargo:rerun-if-changed={}", assets.display());

    let mut files = Vec::new();
    walk(&assets, &assets, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut code = String::from(
        "// 由 build.rs 生成：内嵌资源表（除 assets/charts）。\n\
         pub static FILES: &[(&str, &[u8])] = &[\n",
    );
    for (key, _abs) in &files {
        code.push_str(&format!(
            "    (\"{key}\", include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/assets/{key}\"))),\n"
        ));
    }
    code.push_str("];\n");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    fs::write(Path::new(&out_dir).join("embedded_assets.rs"), code).expect("写入内嵌资源表失败");
}
