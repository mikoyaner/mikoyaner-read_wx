//! WeFlow 第三方可插拔组件共享核心库。
//!
//! 模块规划（随实现逐步补充）：
//! - `db`：SQLCipher 打开/句柄管理、微信 4.x db_storage 布局探查
//! - `schema`：message/session/contact/sns/emoticon/hardlink 各库表结构常量与查询
//! - `dat`：微信 .dat V1/V2/V3 媒体解密与重新加密、WXGF 容器识别
//! - `hardlink`：图片/视频 hardlink 文件解析
//! - `voice`：语音（silk）数据读取
//! - `json`：C ABI 出参 JSON 序列化的公共约定

pub mod dat;
pub mod db;
pub mod keystore;
pub mod messages;
pub mod schema;
// ① 实现方式：五个 `pub mod` 直接把子模块挂到 crate 根，不做 prelude 重导出，
//    调用方一律写全路径（`weflow_core::db::Account`）。
// ② 关键程序解析：模块顺序 = 依赖顺序的倒序书写（dat 无内部依赖；db 依赖 keystore/schema；
//    messages 依赖 db + schema）。`hardlink`/`voice`/`json` 目前只在 //! 规划里，
//    还没有对应文件——看到文档里提到但这里没 mod，不是漏写，是尚未落地。
// ③ 功能：对外暴露「解密媒体 / 开库 / 取钥 / 读消息 / 探 schema」五件套，
//    供 wcdb-native、welive-export、reply-exporter、key-provider 四个上层 crate 共用。

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
    // ① 实现方式：`env!` 在**编译期**读 Cargo 注入的 CARGO_PKG_VERSION，展开成 &'static str 字面量。
    // ② 关键程序解析：不是运行时读环境变量，所以二进制里烧死的是构建时 workspace 版本号；
    //    改了 Cargo.toml 不重新编译，这里仍返回旧值。返回 'static 生命周期，调用方可直接塞进 JSON。
    // ③ 功能：给上层的 version 探针/启动横幅一个统一的库版本口径。
}
