//! 静态 task graph 声明：转诊单生命周期的 4 阶段映射。
//!
//! 对应源产品 copper-rs 的「静态 task graph 声明 + 编译期 wire」主路径：
//!   `core/cu29_runtime/src/curuntime.rs::CuExecutionLoop`
//!   + `core/cu29_derive/src/resources.rs`
//!
//! 该模块只承担「形状声明」，运行时调度/状态推进由后续 task 接入
//! (task 3/5/10/15/24)：

pub mod lifecycle;
