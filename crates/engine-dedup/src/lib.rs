//! L1 `engine-dedup` — fileHash 精确去重（D2 导入即用）+ pHash 汉明距离 ≤8 近似重复
//! （M5 判定 / W5 web 分组）。纯算法零 IO：SQL 组装在 svc-library（L2），HTTP 壳在 app-web。
//!
//! KI-2c：pHash 从「单一取值」升级为 **D4 八变体**（4 旋转 × 是否镜像）——
//! 同一张图被旋转 90/180/270° 或镜像后 pHash 完全变化，单值比对是固定漏判
//! （手机旋转、导出翻转都会撞上）。变体计算 `d4_phashes`、比对 `best_variant_match`、
//! 分组 `build_dup_groups_d4`，命中变换（`旋转 90°` / `水平镜像` …）写进边证据。
//! 老库只有单一 pHash → 视为「只有一个变体」照常参与比对，语义不变。

/// pHash 近似重复判定阈值（PLAN.md M5：≤8 判近似重复）。
pub const PHASH_THRESHOLD: u32 = 8;

/// 精确重复判定：SHA-256 相等。
pub fn is_exact_dup(a: &str, b: &str) -> bool {
    a == b && !a.is_empty()
}

/// 近似重复判定：pHash 汉明距离 ≤ 阈值。
pub fn is_near_dup(pa: u64, pb: u64) -> bool {
    core_hash::hamming(pa, pb) <= PHASH_THRESHOLD
}

// ---------- KI-2c：D4 八变体 pHash（旋转/镜像不再漏判） ----------

/// 正方形对称群 D4 的 8 个元素。**`index()` 即变体数组下标**（0 = identity，
/// 存储文本里的顺序就是它，改顺序等于让存量数据错位 → 永远只往后追加）。
///
/// 元素代数表示 `(k, j)` = **先水平翻转（j=1 时），再顺时针旋转 k×90°**，
/// 与像素层 `apply_grid` 逐字节一致（单测用「组合律 = 逐次 apply」对拍）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum D4 {
    /// 无变换（原始朝向）
    Id,
    /// 顺时针旋转 90°
    Rot90,
    /// 旋转 180°
    Rot180,
    /// 顺时针旋转 270°（= 逆时针 90°）
    Rot270,
    /// 水平镜像（左右翻转）
    MirrorH,
    /// 主对角镜像（转置）
    Transpose,
    /// 垂直镜像（上下翻转）
    MirrorV,
    /// 副对角镜像
    AntiTranspose,
}

/// 八变体的固定顺序（下标 = 存储位置）。顺序冻结，不许重排。
pub const D4_ALL: [D4; 8] = [
    D4::Id,
    D4::Rot90,
    D4::Rot180,
    D4::Rot270,
    D4::MirrorH,
    D4::Transpose,
    D4::MirrorV,
    D4::AntiTranspose,
];

impl D4 {
    /// 变体下标（0..8）。
    pub fn index(self) -> usize {
        match self {
            D4::Id => 0,
            D4::Rot90 => 1,
            D4::Rot180 => 2,
            D4::Rot270 => 3,
            D4::MirrorH => 4,
            D4::Transpose => 5,
            D4::MirrorV => 6,
            D4::AntiTranspose => 7,
        }
    }

    /// 下标 → 变换（越界/未知 → None，脏数据不 panic）。
    pub fn from_index(i: usize) -> Option<D4> {
        D4_ALL.get(i).copied()
    }

    /// 中文文案（界面证据/导出报告用；不暴露给判据逻辑）。
    pub fn label(self) -> &'static str {
        match self {
            D4::Id => "无变换",
            D4::Rot90 => "旋转 90°",
            D4::Rot180 => "旋转 180°",
            D4::Rot270 => "旋转 270°",
            D4::MirrorH => "水平镜像",
            D4::Transpose => "主对角镜像",
            D4::MirrorV => "垂直镜像",
            D4::AntiTranspose => "副对角镜像",
        }
    }

    /// `(k, j)`：先水平翻转 j 次，再顺时针旋转 k×90°。
    fn kj(self) -> (u8, bool) {
        match self {
            D4::Id => (0, false),
            D4::Rot90 => (1, false),
            D4::Rot180 => (2, false),
            D4::Rot270 => (3, false),
            D4::MirrorH => (0, true),
            D4::Transpose => (1, true),
            D4::MirrorV => (2, true),
            D4::AntiTranspose => (3, true),
        }
    }

    fn from_kj(k: u8, j: bool) -> D4 {
        match (k % 4, j) {
            (0, false) => D4::Id,
            (1, false) => D4::Rot90,
            (2, false) => D4::Rot180,
            (3, false) => D4::Rot270,
            (0, true) => D4::MirrorH,
            (1, true) => D4::Transpose,
            (2, true) => D4::MirrorV,
            _ => D4::AntiTranspose,
        }
    }

    /// 群乘法 `self ∘ rhs`（**先施加 rhs，再施加 self**）。
    /// 借助关系 `F·R^k = R^-k·F` 把翻转穿过旋转。
    pub fn compose(self, rhs: D4) -> D4 {
        let (k1, j1) = self.kj();
        let (k2, j2) = rhs.kj();
        let k = if j1 { (k1 + 4 - k2 % 4) % 4 } else { (k1 + k2) % 4 };
        D4::from_kj(k, j1 ^ j2)
    }

    /// 逆元（反射都是二阶元素，自逆）。
    pub fn inverse(self) -> D4 {
        let (k, j) = self.kj();
        if j {
            self
        } else {
            D4::from_kj((4 - k) % 4, false)
        }
    }

    /// 对 n×n 灰度网格（行主序，len = n²）施加变换；长度不是完全平方 → 返回空
    /// （调用方按「无网格」处理，不 panic、不猜）。
    pub fn apply_grid(self, grid: &[u8]) -> Vec<u8> {
        let Some(n) = grid_side(grid.len()) else {
            return Vec::new();
        };
        let (k, j) = self.kj();
        let mut g = if j {
            flip_horizontal(grid, n)
        } else {
            grid.to_vec()
        };
        for _ in 0..k {
            g = rotate90_cw(&g, n);
        }
        g
    }
}

fn grid_side(len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let n = (len as f64).sqrt() as usize;
    (n * n == len).then_some(n)
}

/// 顺时针 90°：`new(x,y) = old(y, n-1-x)`。
fn rotate90_cw(g: &[u8], n: usize) -> Vec<u8> {
    let mut out = vec![0u8; g.len()];
    for y in 0..n {
        for x in 0..n {
            out[y * n + x] = g[(n - 1 - x) * n + y];
        }
    }
    out
}

/// 水平镜像（左右翻转）：`new(x,y) = old(n-1-x, y)`。
fn flip_horizontal(g: &[u8], n: usize) -> Vec<u8> {
    let mut out = vec![0u8; g.len()];
    for y in 0..n {
        for x in 0..n {
            out[y * n + x] = g[y * n + (n - 1 - x)];
        }
    }
    out
}

/// 32×32 灰度网格 → D4 八变体 pHash（下标对齐 `D4_ALL`；0 号 = identity，
/// 与老库 `pHash` 列同一算法、同一取值）。
///
/// 变换施加在**已缩放的 32×32 网格**上：`core_media::luma32` 的等比缩放按轴可分离，
/// 且 32×32 画布居中填充在翻转/转置下位置对称 ⇒ 「转网格」与「转原图再缩」等价，
/// 不必为 8 个变体重复解码（解码是导入最贵的一步）。成本：8× DCT = 毫秒级。
pub fn d4_phashes(grid: &[u8]) -> Result<[u64; 8], core_hash::HashError> {
    let mut out = [0u64; 8];
    // 先算 identity：顺带把网格长度校验交给 core_hash（错长度立刻响亮失败）
    out[0] = core_hash::phash(grid)?;
    for (i, t) in D4_ALL.iter().enumerate().skip(1) {
        out[i] = core_hash::phash(&t.apply_grid(grid))?;
    }
    Ok(out)
}

/// 变体十六进制（16 字符小写，与 `core_hash::phash_hex` 同格式）。
pub fn phash_hex_of(h: u64) -> String {
    format!("{h:016x}")
}

/// 变体集合 → 存库文本（JSON 数组，16 位十六进制小写字符串，下标顺序 = `D4_ALL`）。
/// 选 TEXT+JSON 而不是 8 个定长列：变体数将来若扩（如 16 变体）不用再迁移表结构，
/// 且 `pHash` 老列原样保留为 identity 变体 → 旧包读旧列照常工作（向前兼容）。
pub fn phashes_to_text(hashes: &[u64]) -> String {
    let mut s = String::with_capacity(hashes.len() * 20 + 2);
    s.push('[');
    for (i, h) in hashes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        s.push_str(&phash_hex_of(*h));
        s.push('"');
    }
    s.push(']');
    s
}

/// 存库文本 → 变体集合。容错口径（宁可少判、不许误判为「无 pHash」）：
/// 容忍 `["a","b"]` / `a,b` / 带空白；非法 token 跳过；全空 → 空集合。
pub fn parse_phashes(text: &str) -> Vec<u64> {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | '"' | '\'' | ' ' | '\t' | '\n' | '\r'))
        .collect();
    cleaned
        .split(',')
        .filter(|t| !t.is_empty())
        .filter_map(|t| u64::from_str_radix(t, 16).ok())
        .collect()
}

/// 读取口径（老库兼容，**不得**把只有 `pHash` 的老数据退化成「无 pHash」）：
/// 优先 `pHashes` 多值列；缺失/解析不出 → 回落到单一 `pHash` = identity 变体。
pub fn phashes_for_compare(p_hashes_text: Option<&str>, p_hash: Option<&str>) -> Vec<u64> {
    let multi = p_hashes_text.map(parse_phashes).unwrap_or_default();
    if !multi.is_empty() {
        return multi;
    }
    p_hash.map(parse_phashes).unwrap_or_default()
}

/// 两资产 D4 变体集合的比对结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariantMatch {
    /// 8×8 组合里的**最小**汉明距离
    pub distance: u32,
    /// `b` 相对 `a` 的变换（`b ≈ transform(a)`），用于证据文案「旋转 90°」等
    pub transform: D4,
    /// 取到最小距离时 a 侧用的变体下标
    pub a_variant: usize,
    /// 同上，b 侧
    pub b_variant: usize,
}

/// 两变体集合的最小汉明距离 + 命中变换（空集合 = 该资产没有可用 pHash → None）。
///
/// 复杂度：`O(n_a × n_b)` 次汉明（最坏 8×8 = 64 次，`count_ones` 单指令）；
/// 老数据（各 1 个变体）恒为 1 次，与升级前同代价。
/// 早退：距离 0 已是最小值，且**外层 t 升序**保证先命中 identity 侧
/// （t=0）→ 文案稳定给出「a 侧原样、取 b 的哪个变体」，不会因遍历顺序抖动。
/// 并列时取先遇到的那对（t 优先小、s 其次小），保证同一对资产永远给同一结论。
pub fn best_variant_match(a: &[u64], b: &[u64]) -> Option<VariantMatch> {
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let mut best: Option<VariantMatch> = None;
    for (t, hb) in b.iter().enumerate() {
        // D4 变体最多 8 个；超出的下标（脏数据）不参与变换推断，只按 identity 记
        let tb = D4::from_index(t);
        for (s, ha) in a.iter().enumerate() {
            let d = core_hash::hamming(*ha, *hb);
            let better = match &best {
                None => true,
                Some(m) => d < m.distance,
            };
            if better {
                let transform = match (D4::from_index(s), tb) {
                    (Some(sa), Some(tb)) => tb.inverse().compose(sa),
                    _ => D4::Id,
                };
                best = Some(VariantMatch {
                    distance: d,
                    transform,
                    a_variant: s,
                    b_variant: t,
                });
            }
            if d == 0 {
                return best;
            }
        }
    }
    best
}

// ---------- W5：去重分组（精确 ∪ 近似，union-find 连通分量） ----------

/// 分组输入成员（svc-library 从 assets × resource(primary) 组装）。
#[derive(Debug, Clone)]
pub struct DupMember {
    pub asset_id: i64,
    /// SHA-256；None/空 = 未知，不参与精确边（只可能被 pHash 边连入）。
    pub file_hash: Option<String>,
    /// 64bit pHash；None = 缩略图/pHash 缺失，不参与近似边。
    pub phash: Option<u64>,
}

/// KI-2c：带 D4 变体集合的分组输入成员。
///
/// 为什么另起一个类型而不是给 `DupMember` 加字段：`DupMember` 是既有公开结构，
/// 加字段会让所有 struct literal 构造点（svc-library 的 `dup_groups`）编译不过 →
/// 并行卡的分支冲突+断编译。新类型 + `From<&DupMember>` 让老调用方零改动升级：
/// 老数据只有 identity 变体，`build_dup_groups` 的语义与升级前逐字节一致。
#[derive(Debug, Clone)]
pub struct DupMemberD4 {
    pub asset_id: i64,
    /// SHA-256；None/空 = 未知，不参与精确边。
    pub file_hash: Option<String>,
    /// D4 八变体 pHash（下标 = `D4_ALL`）；空 = 无 pHash。老库单值 → 只放 0 号。
    pub phashes: Vec<u64>,
}

impl DupMemberD4 {
    /// 老数据/缺变体列的入口：单一 pHash 即 identity 变体。
    pub fn from_single(asset_id: i64, file_hash: Option<String>, phash: Option<u64>) -> Self {
        Self {
            asset_id,
            file_hash,
            phashes: phash.into_iter().collect(),
        }
    }

    /// 从库列（`pHashes` 文本 + 老 `pHash`）组装，老数据自动回落单值。
    pub fn from_columns(
        asset_id: i64,
        file_hash: Option<String>,
        p_hashes_text: Option<&str>,
        p_hash: Option<&str>,
    ) -> Self {
        Self {
            asset_id,
            file_hash,
            phashes: phashes_for_compare(p_hashes_text, p_hash),
        }
    }
}

impl From<&DupMember> for DupMemberD4 {
    fn from(m: &DupMember) -> Self {
        DupMemberD4::from_single(m.asset_id, m.file_hash.clone(), m.phash)
    }
}

/// 组类别：exact = 组内所有成员 fileHash 相同且非空；near = 其余（含混合组）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DupKind {
    Exact,
    Near,
}

impl DupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DupKind::Exact => "exact",
            DupKind::Near => "near",
        }
    }
}

/// 边命中层（KI-2c：`exact`（fileHash）优先，`near`（pHash）其次）——
/// 组类别由「组内是否全为 exact 边可达」推出，逐边的命中层单独留证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DupEdgeLayer {
    Exact,
    Near,
}

impl DupEdgeLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            DupEdgeLayer::Exact => "exact",
            DupEdgeLayer::Near => "near",
        }
    }
}

/// 一条去重边（成员证据）：哪一层连上的 + 8×8 最小汉明距离 + 命中变换。
/// `transform == D4::Id` + `distance == 0` 表示两者在 D4 意义下同向重合
/// （同 fileHash 的精确重复、或同向近似重复）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DupEdge {
    /// 输入下标（a < b，与 `dup_edges: a<b` 口径一致）
    pub a: usize,
    pub b: usize,
    pub layer: DupEdgeLayer,
    /// 8×8 变体组合的最小汉明距离（exact 边恒 0）
    pub distance: u32,
    /// b 相对 a 的变换（`b ≈ transform(a)`；exact 边恒 `D4::Id`）
    pub transform: D4,
    pub a_variant: usize,
    pub b_variant: usize,
}

impl DupEdge {
    /// 证据文案：「近似重复（旋转 90°，汉明距离 2）」这类界面/报告可直接用的串。
    pub fn evidence_label(&self) -> String {
        match self.layer {
            DupEdgeLayer::Exact => "精确重复（SHA-256 相同）".to_string(),
            DupEdgeLayer::Near => format!(
                "近似重复（{}，汉明距离 {}）",
                self.transform.label(),
                self.distance
            ),
        }
    }
}

/// 一个重复组：≥2 个成员的连通分量。`members` 为输入下标（升序）。
/// `edges` 为组内全部边（按 (a,b) 升序）→ 命中层与变换的证据来源。
#[derive(Debug, Clone)]
pub struct DupGroup {
    pub kind: DupKind,
    pub members: Vec<usize>,
    pub edges: Vec<DupEdge>,
}

/// 并查集（路径压缩 + 按大小合并）。
struct Dsu {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl Dsu {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

/// 精确 ∪ 近似去重分组：fileHash 相同连边 ∪ pHash 汉明距离 ≤ threshold 连边，
/// 连通分量即组（≥2 成员才算重复组，单例不返回）。
///
/// 这是 KI-2c 之前的入口（`DupMember` 只有单一 pHash）——**语义与行为保持不变**：
/// 内部转成 `DupMemberD4`（单值 = identity 变体）后走 `build_dup_groups_d4`，
/// 8×8 上限退化成 1×1，代价与耗时同升级前一致。
/// 需要 D4 八变体（旋转/镜像）能力的调用方直接调 `build_dup_groups_d4`。
///
/// kind 判定：组内所有成员 fileHash 相同且非空 → Exact，否则 Near
/// （混合组：精确对 + 近似邻接连成一个组时按 Near 呈现，人工核对更稳）。
pub fn build_dup_groups(members: &[DupMember], threshold: u32) -> Vec<DupGroup> {
    let d4: Vec<DupMemberD4> = members.iter().map(DupMemberD4::from).collect();
    build_dup_groups_d4(&d4, threshold)
}

/// 精确 ∪ 近似去重分组（KI-2c：成员可带 D4 八变体 pHash）。
///
/// 连边口径（不变）：fileHash 相同 → exact 边（短路，不再算 pHash：同 SHA 必同像素）；
/// 否则取两侧变体集合 8×8 的**最小**汉明距离，≤ threshold → near 边。
/// 每条边把「命中层 + 距离 + 变换」写进 `DupGroup::edges`（证据/文案来源）。
///
/// 复杂度：精确边 O(n)；近似边 O(n² × |Va| × |Vb|) 次汉明 ——
/// 老数据 1×1 = 与升级前相同的 O(n²) 次 `u64` 汉明（万级库毫秒~几十毫秒）；
/// 全量 8 变体最坏 64× 常数（`count_ones` 单指令级，且距离 0 立即早退）。
/// 不做 per-asset 全库扫描、不建索引：调用方（svc-library）负责量级与分块策略。
pub fn build_dup_groups_d4(members: &[DupMemberD4], threshold: u32) -> Vec<DupGroup> {
    let n = members.len();
    if n < 2 {
        return Vec::new();
    }
    let mut dsu = Dsu::new(n);
    let mut edges: Vec<DupEdge> = Vec::new();

    // 精确边：同 fileHash（非空）→ 与该 hash 首个出现者连边。
    let mut by_hash: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, m) in members.iter().enumerate() {
        if let Some(h) = m.file_hash.as_deref().filter(|h| !h.is_empty()) {
            match by_hash.get(h) {
                Some(&first) => {
                    dsu.union(first, i);
                    edges.push(DupEdge {
                        a: first.min(i),
                        b: first.max(i),
                        layer: DupEdgeLayer::Exact,
                        distance: 0,
                        transform: D4::Id,
                        a_variant: 0,
                        b_variant: 0,
                    });
                }
                None => {
                    by_hash.insert(h, i);
                }
            }
        }
    }

    // 近似边：D4 八变体 8×8 最小汉明距离 ≤ threshold。
    for i in 0..n {
        if members[i].phashes.is_empty() {
            continue;
        }
        for j in (i + 1)..n {
            if members[j].phashes.is_empty() {
                continue;
            }
            // 同 fileHash 已由 exact 边连上（同 SHA ⇒ 同像素，近似的结论没有新信息）
            let same_hash = match (
                members[i].file_hash.as_deref().filter(|h| !h.is_empty()),
                members[j].file_hash.as_deref().filter(|h| !h.is_empty()),
            ) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            if same_hash {
                continue;
            }
            let Some(m) = best_variant_match(&members[i].phashes, &members[j].phashes) else {
                continue;
            };
            if m.distance <= threshold {
                dsu.union(i, j);
                edges.push(DupEdge {
                    a: i,
                    b: j,
                    layer: DupEdgeLayer::Near,
                    distance: m.distance,
                    transform: m.transform,
                    a_variant: m.a_variant,
                    b_variant: m.b_variant,
                });
            }
        }
    }

    // 收集连通分量（≥2 成员），成员升序（输入序）。
    let mut buckets: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for i in 0..n {
        buckets.entry(dsu.find(i)).or_default().push(i);
    }
    let mut groups: Vec<DupGroup> = buckets
        .into_values()
        .filter(|m| m.len() >= 2)
        .map(|mut group_members| {
            group_members.sort_unstable();
            let kind = group_kind(&group_members, members);
            let mut group_edges: Vec<DupEdge> = edges
                .iter()
                .filter(|e| group_members.binary_search(&e.a).is_ok()
                    && group_members.binary_search(&e.b).is_ok())
                .copied()
                .collect();
            group_edges.sort_unstable_by_key(|e| (e.a, e.b));
            DupGroup {
                kind,
                members: group_members,
                edges: group_edges,
            }
        })
        .collect();
    // 大组在前（人工核对从最冗余的开始）；同大小组按最小 assetId 稳定排序。
    groups.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| min_asset_id(&a.members, members).cmp(&min_asset_id(&b.members, members)))
    });
    groups
}

/// 组内所有成员 fileHash 相同且非空 → Exact；否则 Near（含 None / 混合）。
fn group_kind(group_members: &[usize], all: &[DupMemberD4]) -> DupKind {
    let mut iter = group_members
        .iter()
        .map(|&i| all[i].file_hash.as_deref().filter(|h| !h.is_empty()));
    match iter.next() {
        Some(Some(first)) if iter.all(|h| h == Some(first)) => DupKind::Exact,
        _ => DupKind::Near,
    }
}

fn min_asset_id(group_members: &[usize], all: &[DupMemberD4]) -> i64 {
    group_members
        .iter()
        .filter_map(|&i| all.get(i).map(|m| m.asset_id))
        .min()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: i64, hash: Option<&str>, phash: Option<u64>) -> DupMember {
        DupMember {
            asset_id: id,
            file_hash: hash.map(String::from),
            phash,
        }
    }

    #[test]
    fn thresholds() {
        assert!(is_exact_dup("a", "a"));
        assert!(!is_exact_dup("a", "b"));
        assert!(!is_exact_dup("", "")); // 空 hash 不可判
        let x = 0u64;
        let y = 1u64 << 7; // 距离 1 ≤ 8
        assert!(is_near_dup(x, y));
        // 低 9 位全 1：与 0 汉明距离 9 > 8
        let z = 0x1FFu64;
        assert!(!is_near_dup(x, z));
        // 低 8 位全 1：距离 8 ≤ 8（边界）
        let w = 0xFFu64;
        assert!(is_near_dup(x, w));
    }

    #[test]
    fn exact_pair_merges_and_kind_exact() {
        let ms = vec![
            m(1, Some("h1"), Some(0)),
            m(2, Some("h1"), Some(0)),
            m(3, Some("h2"), Some(0xFFFF_FFFF_FFFF_F000)),
        ];
        let gs = build_dup_groups(&ms, PHASH_THRESHOLD);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].kind, DupKind::Exact);
        assert_eq!(gs[0].members, vec![0, 1]);
    }

    #[test]
    fn near_pair_merges_and_kind_near() {
        // pHash 距离 1（不同 fileHash）→ near 组
        let ms = vec![
            m(1, Some("h1"), Some(0)),
            m(2, Some("h2"), Some(1 << 7)),
            m(3, Some("h3"), Some(u64::MAX)),
        ];
        let gs = build_dup_groups(&ms, PHASH_THRESHOLD);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].kind, DupKind::Near);
        assert_eq!(gs[0].members, vec![0, 1]);
    }

    #[test]
    fn chain_transitive_group() {
        // a—b 距 1、b—c 距 1（a—c 距 2）：经传递并成一组（人工核对裁决归属）
        let a = 0u64;
        let b = 1u64;
        let c = b | (1 << 8);
        let ms = vec![
            m(1, Some("h1"), Some(a)),
            m(2, Some("h2"), Some(b)),
            m(3, Some("h3"), Some(c)),
        ];
        let gs = build_dup_groups(&ms, PHASH_THRESHOLD);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].members, vec![0, 1, 2]);
    }

    #[test]
    fn null_phash_merges_only_by_hash() {
        // 无 pHash 的成员只靠 fileHash 连边；不同 hash 且无 pHash → 永不成组
        let ms = vec![
            m(1, Some("h1"), None),
            m(2, Some("h1"), None),
            m(3, Some("h2"), None),
        ];
        let gs = build_dup_groups(&ms, PHASH_THRESHOLD);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].members, vec![0, 1]);
        assert_eq!(gs[0].kind, DupKind::Exact);
    }

    #[test]
    fn mixed_group_is_near() {
        // 精确对 (1,2) + 近似邻接 (2,3)（不同 hash）→ 一组，kind = Near
        let ms = vec![
            m(1, Some("h1"), Some(0)),
            m(2, Some("h1"), Some(0)),
            m(3, Some("h2"), Some(1 << 7)),
        ];
        let gs = build_dup_groups(&ms, PHASH_THRESHOLD);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].members, vec![0, 1, 2]);
        assert_eq!(gs[0].kind, DupKind::Near);
    }

    #[test]
    fn singletons_and_empty_excluded() {
        let ms = vec![m(1, Some("h1"), Some(0)), m(2, Some("h2"), Some(u64::MAX))];
        assert!(build_dup_groups(&ms, PHASH_THRESHOLD).is_empty());
        assert!(build_dup_groups(&[], PHASH_THRESHOLD).is_empty());
        assert!(build_dup_groups(&[m(1, Some("h"), None)], PHASH_THRESHOLD).is_empty());
    }

    #[test]
    fn custom_threshold_respected() {
        let ms = vec![m(1, Some("h1"), Some(0)), m(2, Some("h2"), Some(0xFF))];
        assert_eq!(build_dup_groups(&ms, 8).len(), 1);
        assert!(build_dup_groups(&ms, 7).is_empty()); // 距 8 > 7 → 不组
    }

    #[test]
    fn larger_groups_first() {
        // 两组各自 pHash 相同（0 / 全 1），两组之间距离 64 不连边
        let ms = vec![
            m(1, Some("ha"), Some(0)),
            m(2, Some("ha"), Some(0)),
            m(3, Some("ha"), Some(0)),
            m(4, Some("hb"), Some(u64::MAX)),
            m(5, Some("hb"), Some(u64::MAX)),
        ];
        let gs = build_dup_groups(&ms, PHASH_THRESHOLD);
        assert_eq!(gs.len(), 2);
        assert_eq!(gs[0].members.len(), 3);
        assert_eq!(gs[1].members.len(), 2);
    }

    // ================= KI-2c：D4 八变体 pHash（旋转/镜像不漏判） =================

    /// 32×32 非对称灰度网格（八个 D4 变换互不相同 —— 对称网格会让变体撞成同一个值）。
    fn asym_grid() -> Vec<u8> {
        let n = 32usize;
        let mut g = vec![0u8; n * n];
        for y in 0..n {
            for x in 0..n {
                let v = (x * 37 + y * 91 + (x * y) % 61 + (x * x) % 23) % 256;
                g[y * n + x] = v as u8;
            }
        }
        g
    }

    /// 强条纹 + 亮块：与随机纹理的 pHash 相距很远（当「无关图」用）。
    fn stripes_grid() -> Vec<u8> {
        let n = 32usize;
        let mut g = vec![0u8; n * n];
        for y in 0..n {
            for x in 0..n {
                let striped = ((x / 3) + (y / 3)) % 2 == 0;
                let in_block = (2..10).contains(&x) && (18..30).contains(&y);
                g[y * n + x] = if in_block {
                    250
                } else if striped {
                    200
                } else {
                    15
                };
            }
        }
        g
    }

    fn d4_of(id: i64, grid: &[u8]) -> DupMemberD4 {
        DupMemberD4 {
            asset_id: id,
            file_hash: Some(format!("h{id}")),
            phashes: d4_phashes(grid).unwrap().to_vec(),
        }
    }

    #[test]
    fn d4_compose_matches_pixel_application() {
        // 组合律对拍：代数 compose 必须等于「先 apply rhs 再 apply lhs」的逐字节结果。
        // 这条守住 §D4 的 (k,j) 代数，防止将来改变换定义时只改一半。
        let g = asym_grid();
        for a in D4_ALL {
            for b in D4_ALL {
                let algebra = a.compose(b).apply_grid(&g);
                let pixels = a.apply_grid(&b.apply_grid(&g));
                assert_eq!(
                    algebra,
                    pixels,
                    "{:?} ∘ {:?} 的代数结果与像素结果不一致",
                    a,
                    b
                );
            }
        }
    }

    #[test]
    fn d4_inverse_roundtrips_and_variants_are_distinct() {
        let g = asym_grid();
        let mut seen = std::collections::HashSet::new();
        for t in D4_ALL {
            let tg = t.apply_grid(&g);
            assert_eq!(t.inverse().apply_grid(&tg), g, "{:?} 逆元回不去", t);
            assert_eq!(t.apply_grid(&t.inverse().apply_grid(&g)), g);
            assert!(seen.insert(tg), "{:?} 与已有变体撞了（网格不够非对称）", t);
        }
        assert_eq!(seen.len(), 8, "八变体必须互不相同");
        // 非法网格长度不 panic、返回空（调用方按「无网格」处理）
        assert!(D4::Rot90.apply_grid(&[0u8; 7]).is_empty());
        assert!(D4::Rot90.apply_grid(&[]).is_empty());
    }

    #[test]
    fn d4_rotated_or_mirrored_grid_matches_at_distance_zero() {
        // 同一张图的 4 旋转 + 4 镜像：8×8 最小汉明距离必须是 0，且变换名对得上
        let g = asym_grid();
        let base = d4_phashes(&g).unwrap();
        for t in D4_ALL {
            let rotated = d4_phashes(&t.apply_grid(&g)).unwrap();
            let m = best_variant_match(&base, &rotated).expect("两侧都有 pHash");
            assert_eq!(m.distance, 0, "{:?} 变换后应精确命中", t);
            assert_eq!(m.transform, t, "{:?} 的证据变换写错", t);
            assert_eq!(m.transform.label(), t.label());
        }
    }

    #[test]
    fn d4_unrelated_grids_stay_above_threshold() {
        let a = d4_phashes(&asym_grid()).unwrap();
        let b = d4_phashes(&stripes_grid()).unwrap();
        let m = best_variant_match(&a, &b).unwrap();
        assert!(
            m.distance > PHASH_THRESHOLD,
            "无关图不该成近邻，最小距离 {}",
            m.distance
        );
        // 无关图旋转之后也还是无关（八变体不引入误判）
        for t in D4_ALL {
            let bt = d4_phashes(&t.apply_grid(&stripes_grid())).unwrap();
            let m = best_variant_match(&a, &bt).unwrap();
            assert!(
                m.distance > PHASH_THRESHOLD,
                "无关图旋转 {:?} 后误判成近邻（距离 {}）",
                t,
                m.distance
            );
        }
    }

    #[test]
    fn legacy_single_variant_keeps_old_semantics() {
        // 老库：只有 identity 变体。8×8 退化 1×1 → 与升级前的 hamming 判据完全一致
        let a = vec![0u64];
        let b = vec![1u64 << 7];
        assert_eq!(best_variant_match(&a, &b).unwrap().distance, 1);
        assert_eq!(best_variant_match(&a, &b).unwrap().transform, D4::Id);
        assert!(best_variant_match(&[], &b).is_none());
        assert!(best_variant_match(&a, &[]).is_none());

        // 分组层同样不变：老入口与显式 D4 入口（单值）结果一致
        let ms = vec![
            m(1, Some("h1"), Some(0)),
            m(2, Some("h1"), Some(1 << 7)),
            m(3, Some("h2"), Some(1 << 7)),
            m(4, Some("h3"), Some(u64::MAX)),
        ];
        let legacy = build_dup_groups(&ms, PHASH_THRESHOLD);
        let d4m: Vec<DupMemberD4> = ms
            .iter()
            .map(|x| DupMemberD4::from_single(x.asset_id, x.file_hash.clone(), x.phash))
            .collect();
        let explicit = build_dup_groups_d4(&d4m, PHASH_THRESHOLD);
        assert_eq!(legacy.len(), explicit.len());
        for (l, e) in legacy.iter().zip(explicit.iter()) {
            assert_eq!(l.members, e.members);
            assert_eq!(l.kind, e.kind);
        }
        // 老数据里出现「精确 + 近似」混合边时，edges 证据仍逐条留档
        assert!(legacy
            .iter()
            .flat_map(|g| g.edges.iter())
            .any(|e| e.layer == DupEdgeLayer::Exact));
    }

    #[test]
    fn rotated_pair_forms_near_group_with_evidence() {
        // A 与 B(=A 旋转 90°) 必须成 near 组，边证据写着「旋转 90°」；无关图 C 不进组
        let g = asym_grid();
        let a = d4_of(1, &g);
        let b = d4_of(2, &D4::Rot90.apply_grid(&g));
        let c = d4_of(3, &stripes_grid());
        let gs = build_dup_groups_d4(&[a, b, c], PHASH_THRESHOLD);
        assert_eq!(gs.len(), 1, "只有旋转对成组");
        assert_eq!(gs[0].members, vec![0, 1]);
        assert_eq!(gs[0].kind, DupKind::Near);
        assert_eq!(gs[0].edges.len(), 1);
        let e = gs[0].edges[0];
        assert_eq!(e.layer, DupEdgeLayer::Near);
        assert_eq!(e.transform, D4::Rot90);
        assert_eq!(e.distance, 0);
        assert_eq!(e.evidence_label(), "近似重复（旋转 90°，汉明距离 0）");
    }

    #[test]
    fn single_variant_side_still_misses_rotation_hence_eight_variants() {
        // 升级动机的负向对照：只存 identity 的老数据 vs 旋转图 → 距离 > 8（漏判），
        // 而八变体两侧比对 → 0（判出）。这条防止「回退成单值也能过测试」的假绿。
        let g = asym_grid();
        let legacy_a = vec![d4_phashes(&g).unwrap()[0]];
        // 老数据两侧都只有 identity 变体：旋转图存下来的就是它「转过来之后」的那一个值
        let legacy_b = vec![d4_phashes(&D4::Rot90.apply_grid(&g)).unwrap()[0]];
        let rotated_b = d4_phashes(&D4::Rot90.apply_grid(&g)).unwrap().to_vec();
        let miss = best_variant_match(&legacy_a, &legacy_b).unwrap();
        assert!(
            miss.distance > PHASH_THRESHOLD,
            "单值比对本应漏判（距离 {}），测试图不够非对称？",
            miss.distance
        );
        let hit = best_variant_match(
            &d4_phashes(&g).unwrap().to_vec(),
            &rotated_b,
        )
        .unwrap();
        assert_eq!(hit.distance, 0);
        assert_eq!(hit.transform, D4::Rot90);
    }

    #[test]
    fn phashes_text_roundtrip_and_tolerant_parse() {
        let hs = d4_phashes(&asym_grid()).unwrap();
        let text = phashes_to_text(&hs);
        assert!(text.starts_with('[') && text.ends_with(']'));
        let tokens: Vec<&str> = text
            .trim_matches(|c| c == '[' || c == ']')
            .split(',')
            .collect();
        assert_eq!(tokens.len(), 8, "八变体都应落库: {text}");
        for t in &tokens {
            let lit = t.trim_matches('"');
            assert_eq!(lit.len(), 16, "每项应是 16 位十六进制字符串: {t}");
            assert!(lit.chars().all(|c| c.is_ascii_hexdigit()), "{t} 不是十六进制");
        }
        assert_eq!(parse_phashes(&text), hs.to_vec());
        // 老格式/人工手写也要能吃：无引号、空格、单值
        let plain = hs.iter().map(|h| phash_hex_of(*h)).collect::<Vec<_>>().join(", ");
        assert_eq!(parse_phashes(&plain), hs.to_vec());
        assert_eq!(parse_phashes("  "), Vec::<u64>::new());
        assert_eq!(parse_phashes("[\"zz\",\"00\"]"), vec![0]);
        // 读列口径：多值列优先，缺失回落老单值（不得退化成空 = 「无 pHash」）
        assert_eq!(phashes_for_compare(Some(&text), Some("0000000000000000")), hs.to_vec());
        assert_eq!(phashes_for_compare(None, Some("0000000000000007")), vec![7]);
        assert_eq!(phashes_for_compare(Some("[]"), Some("0000000000000007")), vec![7]);
        assert!(phashes_for_compare(None, None).is_empty());
        // 列组装入口等价
        let mm = DupMemberD4::from_columns(9, Some("h".into()), None, Some("0000000000000007"));
        assert_eq!(mm.phashes, vec![7]);
    }
}
