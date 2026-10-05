//! 编译期内嵌的资源（`assets/` 下除 `charts/` 外的全部文件）。
//!
//! 资源表由 `build.rs` 生成，键为相对 `assets/` 的路径（正斜杠）。

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

/// 按资源键（相对 `assets/`，正斜杠）取内嵌字节；不存在返回 `None`。
pub fn get(key: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|(k, _)| *k == key).map(|(_, data)| *data)
}

/// 同 [`get`]，缺失时 panic（附带键名）。
pub fn expect(key: &str) -> &'static [u8] {
    get(key).unwrap_or_else(|| panic!("内嵌资源缺失: {key}"))
}
