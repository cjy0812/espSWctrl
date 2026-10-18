//! esp-idf-svc 应用的构建脚本。
//!
//! `espidf::sysenv::output()` 会把 IDF 的 include 路径、链接参数、
//! 以及 sdkconfig 里的各项配置导出成 cargo 的 `cargo:rustc-*` 指令。
//! 少了它，esp-idf-sys 的绑定会以「配置全默认」生成，
//! 于是 USB-Serial-JTAG console 之类的项目级设置全部失效。

fn main() -> Result<(), Box<dyn std::error::Error>> {
    embuild::espidf::sysenv::output();
    Ok(())
}
