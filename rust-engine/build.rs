//! 构建脚本。
//!
//! 历史上它负责生成一个含 `#![windows_subsystem]` 的小文件、由 main.rs 用
//! `include!` 注入。那个做法是错的：`include!` 把内层属性放到了文件中部，
//! rustc 报 "an inner attribute is not permitted in this context"。
//! 现在子系统属性由 main.rs 自己用 `#![cfg_attr(..., windows_subsystem = ...)]`
//! 声明，构建脚本只剩下一件事：声明自定义 cfg。

fn main() {
    // 自定义 cfg `engine_uia`：控制引擎上报 "UIA 是否可用"。
    // 没有这一行，启用 uia feature 时 rustc 会对 cfg 名给出 unexpected_cfgs 警告。
    println!("cargo:rustc-check-cfg=cfg(engine_uia)");
    println!("cargo:rerun-if-changed=build.rs");
}
