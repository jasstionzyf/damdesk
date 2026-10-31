//! L1 `engine-restore` — 图像修复推理（C4，PLAN.md §6.3「C4 其余三个官方修复插件」）。
//!
//! 模型（卡内裁决沿用 C3 模式：推理独立 crate，分发统一在 engine-embed::model_store）：
//! - `realesr-general-x4v3`：Real-ESRGAN 官方权重的 ONNX 再打包（SRVGGNetCompact，
//!   1.21M 参数 ~4.7MB，BSD-3-Clause），动态输入 [1,3,H,W] f32（0..1），输出 4 倍
//!   不裁剪（消费端 clamp）——随安装包内置分发（tauri bundle.resources）。
//! - `lama-fp32`：big-lama 局部重绘 ONNX FP32（Apache-2.0，Carve/LaMa-ONNX 导出，
//!   ~208MB，输入固定 512²，image 0..255 + mask {0,1}）——体积原因不随安装包内置，
//!   走 model_store 分发（自建镜像 / file://）。
//!
//! 会话缓存 / 模型根解析 / 模型缺省报错语义全部与 engine-segment 同模式（C3 先例：
//! 两 L1 crate 类型独立禁互引，helpers 各自持有）。`<root>/models/<model>/<model>.onnx`。
//! **进程级全局态**：测试里改根必须走 `ModelsRootScope`（取 `core_config::models_root_lock()`
//! + drop 还原），断言「当前无根」的读侧用例取 `ModelsRootScope::enter()` 与写侧互斥
//! —— 裸 `set_models_root()` / `clear_models_root()` 会与同二进制并行的用例互相踩（卡 task）。
//!
//! 独显高性能档（卡验收原文「检测独显切高性能档」）：`detect_device()` 探测独显
//! （DAM_DEVICE 环境变量可覆盖，测试用）；`cuda` cargo feature 开启时（需带 CUDA
//! 运行时的打包环境）会话优先注册 ORT CUDA EP、构建失败回退 CPU。dam-dev 容器
//! 无独显：CPU 档全量验证，CUDA 档代码就绪但未在本环境验证（如实标注，不虚报）。

#[cfg(feature = "local-infer")]
pub mod inpaint;
#[cfg(feature = "local-infer")]
pub mod upscale;

use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("模型根未设置：set_models_root() 或环境变量 DAM_MODELS_ROOT")]
    NoModelsRoot,
    #[error("model files missing: {0}")]
    ModelMissing(String),
    #[error("onnx: {0}")]
    Onnx(String),
    #[error("{plugin}: 参数错误: {msg}")]
    BadParams { plugin: &'static str, msg: String },
    #[error("{plugin}: {msg}")]
    Failed { plugin: &'static str, msg: String },
}

pub type Result<T> = std::result::Result<T, RestoreError>;

/// realesr-general-x4v3（registry/models.yaml `kind: super_resolution` 条目名）。
pub const REALESR_GENERAL_X4V3: &str = "realesr-general-x4v3";
/// big-lama FP32（registry/models.yaml `kind: inpainting` 条目名）。
pub const LAMA_FP32: &str = "lama-fp32";

// ---------- 模型根解析（与 engine-segment / engine-embed factory 契约一致） ----------

static MODELS_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// 显式注入模型根（桌面壳启动时调用）。
pub fn set_models_root(dir: impl Into<PathBuf>) {
    *MODELS_ROOT.write().expect("models root lock") = Some(dir.into());
}

/// 清除注入（测试隔离用）。
pub fn clear_models_root() {
    *MODELS_ROOT.write().expect("models root lock") = None;
}

/// 生效模型根：显式注入 > `DAM_MODELS_ROOT` 环境变量 > 报错（响亮，不猜默认路径）。
pub fn models_root() -> Result<PathBuf> {
    if let Some(p) = MODELS_ROOT.read().expect("models root lock").clone() {
        return Ok(p);
    }
    if let Ok(env) = std::env::var("DAM_MODELS_ROOT") {
        if !env.trim().is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    Err(RestoreError::NoModelsRoot)
}

/// 单模型 onnx 文件路径：`<root>/models/<model>/<model>.onnx`。
pub fn model_file(model: &str) -> Result<PathBuf> {
    let root = models_root()?;
    Ok(core_config::model_dir(&root, model).join(format!("{model}.onnx")))
}

/// 模型文件已在盘（快路径；编排器/插件做 model_not_downloaded 预检用）。
pub fn model_ready(model: &str) -> bool {
    match model_file(model) {
        Ok(p) => p.is_file(),
        Err(_) => false,
    }
}

/// 模型根作用域：取进程级锁（`core_config::models_root_lock()`）+ **drop 时还原注入值**。
///
/// 与 `engine-segment::ModelsRootScope` 同一形态（两 L1 crate 禁互引，helpers 各自持有；
/// 锁是同一把 —— 放 L0 是为了让「同时改 segment 根 + restore 根」的用例不会有两把锁的
/// 加锁顺序问题）。为什么需要（卡 task）：`set_models_root()` 是进程级全局态，
/// 同一个测试二进制里并行的用例各自改根/清根会互相踩（本 crate 的
/// `inpaint::tests::real_lama_inpaint_smoke` 与 `upscale::tests::unknown_model_fails_loudly`
/// 就是一例：后者 `clear_models_root()` 会把前者的根清掉）。
///
/// 用法（**一个用例只取一次 scope**：锁不可重入）：
/// - 改根：`let _s = ModelsRootScope::set(dir);` / `ModelsRootScope::clear()`；
/// - 断言「当前没有根」的读侧用例：`let _s = ModelsRootScope::enter();`；
/// - 持锁期间要多次改根：直接调 `set_models_root()` / `clear_models_root()`，drop 统一还原；
/// - 需要同时改两个引擎的根：不要嵌套两个 scope，改用 `core_config::models_root_lock()`。
pub struct ModelsRootScope {
    /// 持锁到 drop（字段仅为生命周期，不做读写）。
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<PathBuf>,
}

impl ModelsRootScope {
    /// 只取锁，不改根（读侧用例用）。
    pub fn enter() -> Self {
        let lock = core_config::models_root_lock();
        let prev = MODELS_ROOT.read().expect("models root lock").clone();
        Self { _lock: lock, prev }
    }

    /// 取锁 + 注入根；drop 时还原为取锁前的值。
    pub fn set(dir: impl Into<PathBuf>) -> Self {
        let scope = Self::enter();
        set_models_root(dir);
        scope
    }

    /// 取锁 + 清空注入（交还 `DAM_MODELS_ROOT` 语义）；drop 时还原。
    pub fn clear() -> Self {
        let scope = Self::enter();
        clear_models_root();
        scope
    }

    /// 取锁前的注入值（读侧断言「进入时是空的」用）。
    pub fn previous(&self) -> Option<&PathBuf> {
        self.prev.as_ref()
    }
}

impl Drop for ModelsRootScope {
    fn drop(&mut self) {
        *MODELS_ROOT.write().expect("models root lock") = self.prev.take();
    }
}

// ---------- 独显检测（全档可用，cuda feature 只影响会话 EP 注册） ----------

/// 推理设备档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    Cuda,
}

/// 独显检测：`DAM_DEVICE` 环境变量显式覆盖（cuda|gpu|cpu，测试/强制档用）；
/// 否则探测 `nvidia-smi -L`（独显存在且可查询 → Cuda）。纯探测、无副作用。
pub fn detect_device() -> Device {
    if let Ok(d) = std::env::var("DAM_DEVICE") {
        match d.trim().to_ascii_lowercase().as_str() {
            "cuda" | "gpu" => return Device::Cuda,
            "cpu" => return Device::Cpu,
            _ => {} // 未知值不猜，落到探测
        }
    }
    if has_nvidia_gpu() {
        Device::Cuda
    } else {
        Device::Cpu
    }
}

fn has_nvidia_gpu() -> bool {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.arg("-L");
    // KI-42：nvidia-smi 是 console 子系统程序，Windows 上不抑制就会弹一个黑窗
    // （与 `pack_host::process::no_console_window` 同一口径；本 crate 不依赖 pack-host，
    //  故就地写三行，语义完全一致：只设 creation_flags，不动 stdio）。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 环境变量的 RAII 还原（断言 panic 也要还原，否则污染同二进制其他用例）。
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        /// 取走（删除）变量，drop 时还原。
        fn take(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn models_root_explicit_overrides_env() {
        // 进程级全局态：取锁（同二进制其他改根/清根用例串行化），drop 还原注入值
        let _scope = ModelsRootScope::enter();
        let _env = EnvGuard::take("DAM_MODELS_ROOT");
        set_models_root("/tmp/r-a");
        assert_eq!(model_file(LAMA_FP32).unwrap().to_string_lossy(), "/tmp/r-a/models/lama-fp32/lama-fp32.onnx");
        set_models_root("/tmp/r-b");
        assert_eq!(model_file(REALESR_GENERAL_X4V3).unwrap().to_string_lossy(), "/tmp/r-b/models/realesr-general-x4v3/realesr-general-x4v3.onnx");
        clear_models_root();
        assert!(matches!(model_file(LAMA_FP32), Err(RestoreError::NoModelsRoot)));
    }

    #[test]
    fn device_env_override_wins() {
        // env 是进程级全局态：RAII 还原（panic 也要还原，别把 DAM_DEVICE 留给同二进制其他用例）
        let _env = EnvGuard::take("DAM_DEVICE");
        std::env::set_var("DAM_DEVICE", "cpu");
        assert_eq!(detect_device(), Device::Cpu);
        std::env::set_var("DAM_DEVICE", "cuda");
        assert_eq!(detect_device(), Device::Cuda);
        std::env::set_var("DAM_DEVICE", "gpu");
        assert_eq!(detect_device(), Device::Cuda);
        std::env::set_var("DAM_DEVICE", "nonsense");
        // 未知值 → 探测（容器无 nvidia-smi → Cpu；本测试不断言方向，只断言不 panic）
        let _ = detect_device();
        std::env::remove_var("DAM_DEVICE");
    }
}
