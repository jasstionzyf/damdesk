//! L1 `engine-segment` — 主体分割（C3，PLAN.md §6.3「C3 分割模型接入 + smart_crop」）。
//!
//! **卡内裁决（2026-10-16）：新建独立 crate，不并入 engine-embed provider registry。**
//! 理由：engine-embed 是 embedding 塔（dims/preprocess=重嵌入锚点/provider factory 按
//! embedder 设置寻址），u2net 产物是掩码而非向量，I/O 与生命周期完全不同；合并会迫使
//! 分割模型吃进 embedding 设置面。模型分发（URL+sha256 manifest）仍统一在
//! engine-embed::model_store 的 manifests() 表登记（分发层单一事实源，app-web 下载
//! 管理照常可见）；registry/models.yaml 同步加 `kind: segmentation` 条目。
//!
//! 模型：u2net ONNX FP32（rembg 同源，~168MB）——2026-10-16 用户裁决：随安装包内置
//! 分发（tauri bundle.resources + models_root_effective 三级解析包内优先）；CPU 推理
//! 输入固定 320×320（权重原生输入档，ORT 内部无需再缩）。
//!
//! 预处理（rembg/u2net 官方口径锁定）：`im / max(im)` → ImageNet mean/std（RGB 序）→
//! CHW。后处理：输出 d0 取 min-max 归一 → 阈值 0.5 二值掩码（320² 网格）。
//! fg_ratio / bbox / crop_window 全部在 320 网格上计算（确定性；最近邻上采样下网格
//! 占比 == 全分辨率占比，检测与修复预测天然同口径）。
//!
//! 会话缓存：全局按模型文件路径缓存 `Arc<Segmenter>`（170MB 权重批处理只加载一次）。
//! 模型根：`set_models_root()` 显式注入（桌面壳 C5/C6 接线用）或环境变量
//! `DAM_MODELS_ROOT`（验收/容器用）；布局契约与 engine-embed factory 一致：
//! `<root>/models/<model>/<model>.onnx`（core_config::model_dir）。
//! **进程级全局态**：测试里改根必须走 `ModelsRootScope`（取 `core_config::models_root_lock()`
//! + drop 还原），断言「当前无根」的读侧用例取 `ModelsRootScope::enter()` 与写侧互斥
//! —— 裸 `set_models_root()` 会与同二进制并行的用例互相踩（卡 t_39b4891a）。

#[cfg(feature = "local-infer")]
pub mod registry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    #[error("模型根未设置：set_models_root() 或环境变量 DAM_MODELS_ROOT")]
    NoModelsRoot,
    #[error("model files missing: {0}")]
    ModelMissing(String),
    #[error("onnx: {0}")]
    Onnx(String),
    #[error("{detector}: 参数错误: {msg}")]
    BadParams {
        detector: &'static str,
        msg: String,
    },
    #[error("{detector}: 输入不符: {msg}")]
    InputMismatch {
        detector: &'static str,
        msg: String,
    },
    #[error("{detector}: {msg}")]
    Failed {
        detector: &'static str,
        msg: String,
    },
}

pub type Result<T> = std::result::Result<T, SegmentError>;

/// u2net（registry/models.yaml `kind: segmentation` 条目名 = manifest 名 = 目录名）。
pub const U2NET: &str = "u2net";
/// u2net 原生输入边长（权重锁定，改 = 换模型）。
pub const SIDE: usize = 320;
/// 掩码二值化阈值（min-max 归一后）。
pub const FG_THRESHOLD: f32 = 0.5;

// ---------- 模型根解析（与 engine-embed factory 的 `<root>/models/<name>` 契约一致） ----------

static MODELS_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// 显式注入模型根（appData 式根，权重落 `<root>/models/<model>/`；桌面壳启动时调用）。
///
/// 测试里改根请用 `ModelsRootScope`（取进程级锁 + drop 还原），不要裸调本函数，
/// 否则会与同二进制并行的其他用例互相踩。
pub fn set_models_root(dir: impl Into<PathBuf>) {
    *MODELS_ROOT.write().expect("models root lock") = Some(dir.into());
}

/// 清除注入（测试隔离用：显式注入 > env，清掉后 env 恢复生效）。
/// 测试里请用 `ModelsRootScope::clear()`（取锁 + 还原），语义见其文档。
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
    Err(SegmentError::NoModelsRoot)
}

/// 单模型 onnx 文件路径：`<root>/models/<model>/<model>.onnx`。
pub fn model_file(model: &str) -> Result<PathBuf> {
    let root = models_root()?;
    Ok(core_config::model_dir(&root, model).join(format!("{model}.onnx")))
}

/// 模型文件已在盘（快路径，仅存在性；供编排器做 skipped(model_not_downloaded) 预检）。
pub fn model_ready(model: &str) -> bool {
    match model_file(model) {
        Ok(p) => p.is_file(),
        Err(_) => false,
    }
}

/// 模型根作用域：取进程级锁（`core_config::models_root_lock()`）+ **drop 时还原注入值**。
///
/// 为什么需要（卡 t_39b4891a）：`set_models_root()` 写的是**进程级全局态**，同一个测试
/// 二进制里并行的用例各自改根会互相踩 —— C3 的真模型 e2e 把根指向真 u2net 目录，C4 的
/// 「空根 → ModelMissing」用例随之假失败（它原先是裸 `set_models_root`，没有锁也没有
/// 还原，中间断言 panic 就漏清理）。
///
/// 用法（**一个用例只取一次 scope**：锁不可重入）：
/// - 要改根：`let _s = ModelsRootScope::set(dir);`（`clear()` 同义）；
/// - 只断言「当前没有根 / 根是 X」的读侧用例：`let _s = ModelsRootScope::enter();`
///   —— 不取锁的读者一样会被写侧带偏；
/// - 持锁期间要多次改根：直接调 `set_models_root()` / `clear_models_root()`，
///   drop 时统一还原为取锁前的值；
/// - 需要**同时**改两个引擎（segment + restore）的根时不要嵌套两个 scope（同一把全局锁
///   → 自锁），改用 `core_config::models_root_lock()` 取一次锁 + 两个裸 set。
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

// ---------- 掩码（320² 网格） ----------

/// 二值前景掩码（SIDE² 网格，data[y*side+x] ∈ {0,1}）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    pub side: usize,
    pub data: Vec<u8>,
}

/// 掩码里的一个 4-连通域（= 画面里的一块实体）。`area` 是它自己的前景 cell 数
/// （网格口径，`area / side²` 就是这块实体占画面的比例）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Component {
    /// 紧 bbox（inclusive 网格坐标 `[x0,y0,x1,y1]`）。
    pub bbox: [usize; 4],
    /// 前景 cell 数（不是 bbox 面积：多实例里 bbox 会互相重叠）。
    pub area: u32,
}

impl Mask {
    fn empty() -> Self {
        Self {
            side: SIDE,
            data: vec![0; SIDE * SIDE],
        }
    }

    /// 前景像素数（网格）。
    pub fn fg_count(&self) -> u32 {
        self.data.iter().map(|&v| v as u32).sum()
    }

    /// 前景占比 = fg / side²（== 最近邻上采样到任意分辨率后的像素占比，同口径铁律）。
    pub fn fg_ratio(&self) -> f64 {
        self.fg_count() as f64 / (self.side * self.side) as f64
    }

    /// 前景包围盒（inclusive 网格坐标 [x0,y0,x1,y1]）；无前景 → None。
    pub fn bbox(&self) -> Option<[usize; 4]> {
        let s = self.side;
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        for y in 0..s {
            for x in 0..s {
                if self.data[y * s + x] == 1 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x0 != usize::MAX).then_some([x0, y0, x1, y1])
    }

    /// 全部 4-连通域（多实例主体），按前景 cell 数**降序**、同面积按扫描序（确定性）。
    ///
    /// 每个连通域给紧 bbox（inclusive 网格坐标）与自己占的 cell 数。多商品场景（一串
    /// 胶囊 / 一排在架商品）在掩码里就是多个连通域：只取最大那个会把其余实体的自带文字
    /// 判成「主体外文字」（K07 咖啡胶囊 f002 的根因），所以「主体区」要按实例集合算。
    pub fn components(&self) -> Vec<Component> {
        let s = self.side;
        let n = s * s;
        let mut visited = vec![false; n];
        let mut stack: Vec<usize> = Vec::new();
        let mut out: Vec<Component> = Vec::new();
        for start in 0..n {
            if self.data[start] == 0 || visited[start] {
                continue;
            }
            // 迭代式 flood fill（4-连通，无递归）
            stack.clear();
            stack.push(start);
            visited[start] = true;
            let (mut x0, mut y0, mut x1, mut y1, mut area) =
                (usize::MAX, usize::MAX, 0usize, 0usize, 0usize);
            while let Some(cur) = stack.pop() {
                let cy = cur / s;
                let cx = cur % s;
                area += 1;
                x0 = x0.min(cx);
                y0 = y0.min(cy);
                x1 = x1.max(cx);
                y1 = y1.max(cy);
                for (dx, dy) in [(1i64, 0), (-1, 0), (0, 1), (0, -1)] {
                    let nx = cx as i64 + dx;
                    let ny = cy as i64 + dy;
                    if nx < 0 || ny < 0 || nx >= s as i64 || ny >= s as i64 {
                        continue;
                    }
                    let ni = ny as usize * s + nx as usize;
                    if self.data[ni] == 1 && !visited[ni] {
                        visited[ni] = true;
                        stack.push(ni);
                    }
                }
            }
            out.push(Component { bbox: [x0, y0, x1, y1], area: area as u32 });
        }
        // 稳定排序：面积降序，同面积保持扫描序（确定性，不依赖 sort_unstable）
        out.sort_by(|a, b| b.area.cmp(&a.area));
        out
    }

    /// 最大 4-连通域（主主体）包围盒。多物体散点场景里全掩码 bbox 跨全图且密度低，
    /// 裁剪无法收敛到目标占比；取主主体的紧 bbox 才是 smart_crop 的正确语义
    /// （切到主主体附近，复检口径仍是全掩码占比，见 `Segmentation::crop_window`）。
    pub fn dominant_bbox(&self) -> Option<[usize; 4]> {
        self.components().first().map(|c| c.bbox)
    }

    /// 窗口（inclusive，越界部分裁掉）内前景数（O(窗口高)，网格级，供 crop_window 反复查值）。
    pub fn fg_in_window(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> u32 {
        let s = self.side;
        let x1 = x1.min(s - 1);
        let y1 = y1.min(s - 1);
        let mut n = 0u32;
        for y in y0..=y1.min(s - 1) {
            n += self.data[y * s + x0..=y * s + x1]
                .iter()
                .map(|&v| v as u32)
                .sum::<u32>();
        }
        n
    }

    /// 最近邻展开到全分辨率（C4 replace_bg 复合底座）：返回 0/255 字节缓冲
    /// （len = w*h）。网格 cell (gx,gy) 覆盖像素块 [gx*w/side, (gx+1)*w/side) —
    /// 与 fg_ratio「最近邻上采样占比不变」同一口径（检测/修复预测同网格铁律）。
    pub fn expand_to(&self, w: u32, h: u32) -> Vec<u8> {
        let s64 = self.side as u64;
        let (w64, h64) = (w as u64, h as u64);
        let mut out = vec![0u8; (w64 * h64) as usize];
        for py in 0..h64 {
            let gy = py * s64 / h64;
            let row = (gy * s64) as usize;
            let out_row = (py * w64) as usize;
            for px in 0..w64 {
                let gx = px * s64 / w64;
                if self.data[row + gx as usize] == 1 {
                    out[out_row + px as usize] = 255;
                }
            }
        }
        out
    }
}

/// 单次分割结果：掩码 + 占比 + 包围盒（全为 320 网格口径）。
#[derive(Debug, Clone, PartialEq)]
pub struct Segmentation {
    pub mask: Mask,
    pub fg_ratio: f64,
    pub bbox: Option<[usize; 4]>,
}

impl Segmentation {
    /// 围绕**主主体（最大连通域）bbox 中心**搜索「掩码占比最接近 target」的裁剪窗
    /// （inclusive 网格坐标）。
    ///
    /// 语义（smart_crop 的数学核心，确定性）：
    /// - 窗口 = bbox 各轴等比放大 s 倍、中心对齐 bbox 中心、钳进网格；
    /// - 比值随 s 单调不增（扩窗只加背景），二分 s 40 轮（固定轮数 = 确定性）取
    ///   |占比-target| 最小的 s；
    /// - 返回 (窗口, 预测占比)。预测占比 = 窗口内前景网格数 / 窗口面积 —— 与复检
    ///   在裁剪产物上重跑 u2net 的口径一致（同阈值同网格），误差只来自模型对取景
    ///   变化的响应，不含实现口径差。
    /// - 无法到达 target（主体过满 bbox 占满全图 / 无前景）时返回可达的最优窗，
    ///   修复成败交由复检裁决（guaranteed 破例 → pending_human 兜底）。
    /// 无前景 → None（调用方响亮报错，不静默输出原图）。
    pub fn crop_window(&self, target: f64) -> Option<([usize; 4], f64)> {
        // 主主体 bbox（散点掩码退化为小噪声块时面积可能小于全掩码 bbox，取较大者的
        // 语义是「最大连通域」——它就是画面主体；散点噪声域面积小不会胜出）
        let [bx0, by0, bx1, by1] = self.mask.dominant_bbox()?;
        let side = self.mask.side;
        let bw = (bx1 - bx0 + 1) as f64;
        let bh = (by1 - by0 + 1) as f64;
        let cx = (bx0 as f64 + bx1 as f64) / 2.0;
        let cy = (by0 as f64 + by1 as f64) / 2.0;
        // s 上界：窗口两轴都不超出网格
        let s_max = f64::min(side as f64 / bw, side as f64 / bh);

        let window_at = |s: f64| -> ([usize; 4], f64) {
            let w = (bw * s).round().max(1.0).min(side as f64) as usize;
            let h = (bh * s).round().max(1.0).min(side as f64) as usize;
            // 中心对齐 bbox 中心（不随 s 漂移）+ 钳进网格（越界整体平移，不缩窗）
            let x0 = ((cx - w as f64 / 2.0).round() as isize).clamp(0, (side - w) as isize) as usize;
            let y0 = ((cy - h as f64 / 2.0).round() as isize).clamp(0, (side - h) as isize) as usize;
            let win = [x0, y0, x0 + w - 1, y0 + h - 1];
            let fg = self.mask.fg_in_window(win[0], win[1], win[2], win[3]);
            let ratio = fg as f64 / (w * h) as f64;
            (win, ratio)
        };

        // 可达域 [ratio(s_max), ratio(1)]；目标在域内 → 二分；在域外 → 取端点（最优可达）
        let mut lo = 1.0f64;
        let mut hi = s_max.max(1.0);
        let mut best: ([usize; 4], f64) = window_at(1.0);
        let mut best_err = (best.1 - target).abs();
        for _ in 0..40 {
            let mid = (lo + hi) / 2.0;
            let (win, ratio) = window_at(mid);
            let err = (ratio - target).abs();
            if err < best_err {
                best_err = err;
                best = (win, ratio);
            }
            // 比值随窗口增大递减：占比偏高 → 扩窗（hi 方向）
            if ratio > target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        // 端点兜底（全图窗 / 紧 bbox 也可能是最优可达）
        for s in [1.0f64, s_max.max(1.0)] {
            let cand = window_at(s);
            if (cand.1 - target).abs() <= best_err {
                best_err = (cand.1 - target).abs();
                best = cand;
            }
        }
        Some(best)
    }
}

// ---------- 推理（local-infer 门控） ----------

#[cfg(feature = "local-infer")]
mod infer {
    use super::*;
    use ort::session::Session;
    use ort::value::Tensor;

    /// 已加载的 u2net 会话（输入/输出名加载期从图里确定，不硬编码字符串）。
    /// Session::run 需要 &mut（ort rc.10），Mutex 包一层支持 Arc 共享（批处理复用会话）。
    pub struct Segmenter {
        session: Mutex<Session>,
        in_name: String,
        out_name: String,
    }

    impl Segmenter {
        /// 从 onnx 文件加载（intra-op 线程 = min(CPU 核数, 8)，防队列并发超订）。
        pub fn load(onnx: &std::path::Path) -> Result<Self> {
            if !onnx.is_file() {
                return Err(SegmentError::ModelMissing(onnx.display().to_string()));
            }
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(8);
            let session = Session::builder()
                .and_then(|b| b.with_intra_threads(threads))
                .and_then(|b| b.commit_from_file(onnx))
                .map_err(|e| SegmentError::Onnx(format!("{}: {e}", onnx.display())))?;
            let in_name = session
                .inputs
                .first()
                .map(|i| i.name.clone())
                .ok_or_else(|| SegmentError::Onnx("u2net has no inputs".into()))?;
            // u2net 图有 7 个输出（d0..d6 侧输出），d0 = 首输出 = 320² 显著性图
            let out_name = session
                .outputs
                .first()
                .map(|o| o.name.clone())
                .ok_or_else(|| SegmentError::Onnx("u2net has no outputs".into()))?;
            Ok(Self {
                session: Mutex::new(session),
                in_name,
                out_name,
            })
        }

        /// RGB 交错缓冲 → 掩码（320² 网格）。预处理口径锁定见模块注释。
        pub fn run_rgb(&self, width: u32, height: u32, pixels: &[u8]) -> Result<Segmentation> {
            let (w, h) = (width as usize, height as usize);
            if w == 0 || h == 0 || pixels.len() != w * h * 3 {
                return Err(SegmentError::InputMismatch {
                    detector: "u2net",
                    msg: format!("RGB 缓冲 {w}x{h} 期望 {} 字节，实际 {}", w * h * 3, pixels.len()),
                });
            }
            let input = preprocess(width, height, pixels);
            let t = Tensor::from_array((vec![1usize, 3, SIDE, SIDE], input.into_boxed_slice()))
                .map_err(|e| SegmentError::Onnx(e.to_string()))?;
            let mut session = self.session.lock().expect("u2net session lock");
            let outputs = session
                .run(ort::inputs![self.in_name.as_str() => t])
                .map_err(|e| SegmentError::Onnx(format!("run: {e}")))?;
            let value = outputs
                .get(self.out_name.as_str())
                .ok_or_else(|| SegmentError::Onnx(format!("output {} missing", self.out_name)))?;
            let arr = value
                .try_extract_array::<f32>()
                .map_err(|e| SegmentError::Onnx(format!("extract: {e}")))?;
            let map: Vec<f32> = arr.iter().copied().collect();
            if map.len() != SIDE * SIDE {
                return Err(SegmentError::Onnx(format!(
                    "d0 形状非 {}²：{} 元素",
                    SIDE,
                    map.len()
                )));
            }
            Ok(postprocess(map))
        }
    }

    /// `im/max(im)` → ImageNet mean/std（RGB）→ CHW f32；面积均值缩放到 SIDE²
    /// （降采样抗锯齿，上采样退化为最近邻——确定性，无外部图像依赖）。
    pub fn preprocess(width: u32, height: u32, pixels: &[u8]) -> Vec<f32> {
        let (w, h) = (width as usize, height as usize);
        let mut max: u8 = 0;
        for &v in pixels {
            max = max.max(v);
        }
        let scale = 1.0f32 / max.max(1) as f32;
        const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
        const STD: [f32; 3] = [0.229, 0.224, 0.225];
        let side = SIDE;
        let mut out = vec![0f32; 3 * side * side];
        let sx = w as f64 / side as f64;
        let sy = h as f64 / side as f64;
        for dy in 0..side {
            // 目标行对应的源行带（面积均值的行分段）
            let y0 = ((dy as f64) * sy).floor() as usize;
            let y1 = (((dy + 1) as f64) * sy).ceil().min(h as f64) as usize;
            for dx in 0..side {
                let x0 = ((dx as f64) * sx).floor() as usize;
                let x1 = (((dx + 1) as f64) * sx).ceil().min(w as f64) as usize;
                // 源带内取面积均值（逐格累加；带内像素数有限，downscale 时即为真面积均值）
                let (mut r, mut g, mut b, mut n) = (0f32, 0f32, 0f32, 0f32);
                for y in y0..y1.max(y0 + 1) {
                    for x in x0..x1.max(x0 + 1) {
                        let idx = (y * w + x) * 3;
                        r += pixels[idx] as f32;
                        g += pixels[idx + 1] as f32;
                        b += pixels[idx + 2] as f32;
                        n += 1.0;
                    }
                }
                let (r, g, b) = (r / n * scale, g / n * scale, b / n * scale);
                let di = dy * side + dx;
                out[di] = (r - MEAN[0]) / STD[0];
                out[side * side + di] = (g - MEAN[1]) / STD[1];
                out[2 * side * side + di] = (b - MEAN[2]) / STD[2];
            }
        }
        out
    }

    /// d0 min-max 归一 → 阈值 0.5 → Mask（rembg 后处理口径）。
    pub fn postprocess(map: Vec<f32>) -> Segmentation {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for &v in &map {
            min = min.min(v);
            max = max.max(v);
        }
        let span = (max - min).max(1e-12);
        let mut mask = Mask::empty();
        for (i, &v) in map.iter().enumerate() {
            if (v - min) / span >= FG_THRESHOLD {
                mask.data[i] = 1;
            }
        }
        let bbox = mask.bbox();
        let fg_ratio = mask.fg_ratio();
        Segmentation { mask, fg_ratio, bbox }
    }
}

#[cfg(feature = "local-infer")]
pub use infer::{postprocess, preprocess, Segmenter};

// ---------- 会话缓存（批处理 170MB 权重只加载一次） ----------

#[cfg(feature = "local-infer")]
static SEGMENTERS: Mutex<Option<HashMap<PathBuf, Arc<Segmenter>>>> = Mutex::new(None);

/// 按模型名取缓存会话（首次调用加载；文件路径为缓存键——换文件即换键）。
#[cfg(feature = "local-infer")]
pub fn segmenter(model: &str) -> Result<Arc<Segmenter>> {
    let path = model_file(model)?;
    let mut guard = SEGMENTERS.lock().expect("segmenter cache lock");
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(s) = cache.get(&path) {
        return Ok(s.clone());
    }
    let s = Arc::new(Segmenter::load(&path)?);
    cache.insert(path, s.clone());
    Ok(s)
}

#[cfg(not(feature = "local-infer"))]
pub fn segmenter(_model: &str) -> Result<std::convert::Infallible> {
    Err(SegmentError::Failed {
        detector: "engine-segment",
        msg: "构建未启用 local-infer（web 容器无 ONNX 运行时），分割不可用".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 环境变量的 RAII 还原（断言 panic 也要还原，否则污染同二进制其他用例；
    /// env 不在 `ModelsRootScope` 的守护范围内，`DAM_MODELS_ROOT` 只由本用例写）。
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
    fn models_root_explicit_overrides_env_and_missing_fails_loud() {
        // 进程级全局态：先取进程级锁（与同二进制其他写根/读根用例串行化），
        // drop 还原注入值 —— 不再手写「清掉」也不怕中途断言 panic。
        let _scope = ModelsRootScope::enter();
        let _env = EnvGuard::take("DAM_MODELS_ROOT");
        let dir = std::env::temp_dir().join(format!("dam-seg-root-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 未设置且未注入 → 响亮报错（不猜默认路径）
        assert!(matches!(models_root(), Err(SegmentError::NoModelsRoot)));
        // 显式注入生效；清掉后回到「无根」（env 也为空）
        set_models_root("/nonexistent-bridge-value");
        assert_eq!(
            models_root().unwrap(),
            PathBuf::from("/nonexistent-bridge-value")
        );
        clear_models_root();
        assert!(matches!(models_root(), Err(SegmentError::NoModelsRoot)));
        // env 兜底
        std::env::set_var("DAM_MODELS_ROOT", &dir);
        assert_eq!(models_root().unwrap(), dir);
        // 布局契约：<root>/models/<model>/<model>.onnx
        assert_eq!(
            model_file(U2NET).unwrap(),
            dir.join("models").join(U2NET).join("u2net.onnx")
        );
        assert!(!model_ready(U2NET));
        std::fs::create_dir_all(dir.join("models").join(U2NET)).unwrap();
        std::fs::write(model_file(U2NET).unwrap(), b"stub").unwrap();
        assert!(model_ready(U2NET));
        // 显式注入优先于环境变量
        let other = dir.join("other");
        set_models_root(&other);
        assert_eq!(model_file(U2NET).unwrap(), other.join("models/u2net/u2net.onnx"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// expand_to 最近邻展开（C4 replace_bg 底座）：占比守恒 + 像素块边界确定。
    #[test]
    fn expand_to_scales_nearest_and_keeps_ratio() {
        let side = SIDE;
        let mut mask = Mask::empty();
        for y in 0..160 {
            for x in 0..320 {
                mask.data[y * side + x] = 1; // 上半屏前景 → ratio = 0.5
            }
        }
        let out = mask.expand_to(640, 320);
        assert_eq!(out.len(), 640 * 320);
        let fg = out.iter().filter(|&&v| v == 255).count();
        assert_eq!(fg, 640 * 160, "上半屏 → 恰好一半像素");
        // 像素块边界：cell(0)（前景）覆盖 [0, 640*160/320) = 前两行每 2px 块
        assert_eq!(out[0], 255);
        assert_eq!(out[639], 255); // 第 159 行 cell → 255
        assert_eq!(out[(160 * 640) as usize], 0, "下半屏背景");
        // 319 行（cell 159.5→159）仍为前景，320 行起为背景
        assert_eq!(out[(159 * 640 + 320) as usize], 255);
    }

    /// 合成掩码的 bbox / 占比 / 裁剪窗数学（无模型，确定性核心）。
    #[test]
    fn mask_bbox_ratio_and_crop_window_math() {
        // 320² 网格，中心 100×100 前景块
        let side = SIDE;
        let mut mask = Mask::empty();
        for y in 110..210 {
            for x in 110..210 {
                mask.data[y * side + x] = 1;
            }
        }
        let seg = Segmentation {
            fg_ratio: mask.fg_ratio(),
            bbox: mask.bbox(),
            mask,
        };
        assert_eq!(seg.bbox, Some([110, 110, 209, 209]));
        let expect = 100.0 * 100.0 / (side * side) as f64;
        assert!((seg.fg_ratio - expect).abs() < 1e-12);

        // 100×100 bbox 占比 9.8% → 裁剪窗应放大到 ~75% 占比（窗口边 ≈ 100/√0.75 ≈ 115）
        let (win, ratio) = seg.crop_window(0.75).unwrap();
        let w = win[2] - win[0] + 1;
        let h = win[3] - win[1] + 1;
        assert!((ratio - 0.75).abs() < 0.02, "ratio={ratio}");
        assert!(w >= 113 && w <= 118, "w={w}");
        assert!(h >= 113 && h <= 118, "h={h}");
        // 窗口完整覆盖 bbox
        assert!(win[0] <= 110 && win[1] <= 110 && win[2] >= 209 && win[3] >= 209);

        // 主体贴右下角（越界场景）：窗口钳进网格，仍不丢前景
        let side = SIDE;
        let mut mask = Mask::empty();
        for y in 280..318 {
            for x in 280..318 {
                mask.data[y * side + x] = 1;
            }
        }
        let seg2 = Segmentation {
            fg_ratio: mask.fg_ratio(),
            bbox: mask.bbox(),
            mask,
        };
        let (win2, _r2) = seg2.crop_window(0.75).unwrap();
        assert!(win2[2] < side && win2[3] < side, "{win2:?}");
        assert!(win2[0] <= 280 && win2[1] <= 280);
        // 主体满幅（bbox = 全图）：裁剪修不了，窗口退化为全图（复检裁决转人工）
        let mut mask = Mask::empty();
        mask.data.fill(1);
        let seg3 = Segmentation {
            fg_ratio: 1.0,
            bbox: mask.bbox(),
            mask,
        };
        let (win3, r3) = seg3.crop_window(0.75).unwrap();
        assert_eq!(win3, [0, 0, side - 1, side - 1]);
        assert!((r3 - 1.0).abs() < 1e-12);

        // 无前景 → None
        let seg4 = Segmentation {
            fg_ratio: 0.0,
            bbox: Mask::empty().bbox(),
            mask: Mask::empty(),
        };
        assert_eq!(seg4.crop_window(0.75), None);
    }

    /// 多物体场景：主主体取最大连通域（crop_window 语义），全掩码 bbox 不用于裁剪。
    #[test]
    fn dominant_bbox_picks_largest_component() {
        let side = SIDE;
        let mut mask = Mask::empty();
        // 小块（30²）在左上，大块（80²）在右下
        for y in 20..50 {
            for x in 20..50 {
                mask.data[y * side + x] = 1;
            }
        }
        for y in 200..280 {
            for x in 210..290 {
                mask.data[y * side + x] = 1;
            }
        }
        assert_eq!(mask.bbox(), Some([20, 20, 289, 279]));
        assert_eq!(mask.dominant_bbox(), Some([210, 200, 289, 279]));
        // 单块场景两者一致
        let mut single = Mask::empty();
        for y in 100..180 {
            for x in 120..200 {
                single.data[y * side + x] = 1;
            }
        }
        assert_eq!(single.dominant_bbox(), single.bbox());
        assert_eq!(Mask::empty().dominant_bbox(), None);
    }

    /// 多实例：`components()` 按面积降序列出全部连通域（多商品图的「主体区」来源），
    /// 同面积按扫描序（确定性），`dominant_bbox()` 仍等于面积最大那块。
    #[test]
    fn components_lists_all_instances_by_area() {
        let side = SIDE;
        let mut mask = Mask::empty();
        // 三块：30²（左上）、80²（右下）、同样 30²（右上，与左上同面积 → 扫描序在前）
        for y in 20..50 {
            for x in 20..50 {
                mask.data[y * side + x] = 1;
            }
        }
        for y in 20..50 {
            for x in 250..280 {
                mask.data[y * side + x] = 1;
            }
        }
        for y in 200..280 {
            for x in 210..290 {
                mask.data[y * side + x] = 1;
            }
        }
        let comps = mask.components();
        assert_eq!(comps.len(), 3, "{comps:?}");
        assert_eq!(comps[0].bbox, [210, 200, 289, 279]);
        assert_eq!(comps[0].area, 80 * 80);
        // 同面积：扫描序（先扫到左上那块）在前
        assert_eq!(comps[1].bbox, [20, 20, 49, 49]);
        assert_eq!(comps[2].bbox, [250, 20, 279, 49]);
        assert_eq!(comps[1].area, comps[2].area);
        assert_eq!(mask.dominant_bbox(), Some(comps[0].bbox));
        // 4-连通口径：对角相邻不算连通（2×2 对角两块 → 2 个连通域）
        let mut diag = Mask::empty();
        diag.data[0] = 1;
        diag.data[side + 1] = 1;
        assert_eq!(diag.components().len(), 2);
        assert!(Mask::empty().components().is_empty());
    }

    /// 预处理数学：纯白图 → 归一化后全通道恒定；缓冲尺寸校验响亮报错。
    #[cfg(feature = "local-infer")]
    #[test]
    fn preprocess_math_and_input_check() {
        let (w, h) = (2usize, 2usize);
        let px = vec![255u8; w * h * 3];
        let t = preprocess(w as u32, h as u32, &px);
        assert_eq!(t.len(), 3 * SIDE * SIDE);
        let side = SIDE;
        // 白图 max=255 → r=g=b=1.0 → (1-mean)/std 恒定
        let r0 = t[0];
        assert!((r0 - (1.0 - 0.485) / 0.229).abs() < 1e-6);
        assert!((t[side * side] - (1.0 - 0.456) / 0.224).abs() < 1e-6);
        assert!((t[2 * side * side] - (1.0 - 0.406) / 0.225).abs() < 1e-6);
        // 确定性
        assert_eq!(preprocess(w as u32, h as u32, &px), t);

        let seg_err = match Segmenter::load(std::path::Path::new("/no/such.onnx")) {
            Err(e) => e,
            Ok(_) => panic!("应报 ModelMissing"),
        };
        assert!(matches!(seg_err, SegmentError::ModelMissing(_)));
    }

    #[cfg(feature = "local-infer")]
    #[test]
    fn postprocess_thresholds_minmax_normalized_map() {
        // 0..1 渐变图：min-max 归一不变 → 阈值 0.5 切一半
        let map: Vec<f32> = (0..SIDE * SIDE)
            .map(|i| i as f32 / (SIDE * SIDE - 1) as f32)
            .collect();
        let seg = postprocess(map);
        let fg = seg.mask.fg_count();
        assert!((fg as f64 / (SIDE * SIDE) as f64 - 0.5).abs() < 0.01, "fg={fg}");
        assert!(seg.bbox.is_some());
        // 常数图：span 兜底 → (v-min)/span=0 → 无前景
        let seg2 = postprocess(vec![0.7f32; SIDE * SIDE]);
        assert_eq!(seg2.mask.fg_count(), 0);
        assert_eq!(seg2.bbox, None);
    }
}
