//! N1 旧「行业包」→ 默认规格的迁移（卡 t_1cde30f2）。
//!
//! 口径（damdesk-product `plugin-design-and-ui.md` §2/§5）：
//! 老库里的「行业包规范」要落成插件下的**默认规格** —— 用户升级后不该看到
//! 「我原来的淘宝主图规范不见了」。
//!
//! IP-K16 起**行业元数据只住处理插件**（内置/导入的 YAML 行业包已删除），所以派生源
//! 从 YAML 换成插件包的 `resources/rules.json`（同一份判据权威，插件运行期也读它）。
//!
//! 三条硬约束（违反即错）：
//! 1. **阈值取包内声明值**：`spec.thresholds` 由 `resources/rules.json` 的
//!    `rules[].params` **逐条派生**（不是重新手抄一遍数字）—— 复制粘贴的数字迟早会和源分叉。
//! 2. **禁止静默丢阈值**：源包里每一个数值型 `check.params` 键都必须落进 `thresholds`；
//!    有代码插件参数 schema 的能力，还必须能折成平铺参数（`paramsDiff`），否则**报错不落库**
//!    （宁可迁移失败让人看见，也不要悄悄少一条阈值）。
//! 3. **幂等**：`originKey`（`legacy-pack:<packId>` / `seed:<name>`）带唯一索引，
//!    跑第二遍不新建、**也不覆盖**用户后来改过的值（重跑 = 校验，不是重置）。
//!
//! 回读校验（`SeedReport::verify`）：写完必须从库里重新读出来，逐项比对
//! 「originKey 唯一 / 阈值 == 派生值 / 规则非空 / 名称非空」。**只有回读通过才算迁移成功**。

use core_storage::specs::{SpecRecord, SpecRepo, SqliteSpecRepo};
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::compliance_api::PLUGIN_REALESTATE_ID;

/// 一条默认规格的迁移来源（也决定幂等键）。
#[derive(Debug, Clone)]
pub struct SeedSpec {
    /// 幂等键：`legacy-pack:<packId>`（旧配置包迁出）或 `seed:<slug>`（无旧包，按插件默认值起一条）
    pub origin_key: String,
    /// 规格名（用户看到的名字：淘宝主图 / 亚马逊主图 / 房源照片 / 抠图白底）
    pub name: String,
    /// 归属能力身份：有代码插件 = 插件 manifest id；无插件的老包 = 配置包 id
    pub plugin_id: String,
    pub industry: Option<String>,
    /// 源配置包 id（无源 = `None`，如 Amazon 从未有过独立 YAML）
    pub source_pack: Option<String>,
    pub rules: Vec<String>,
    pub thresholds: Value,
    pub params_diff: Value,
    /// 动作链（处理类方案的内容；检查类恒空）
    pub actions: Value,
    /// 检查类规格必须带规则（规则子集是它的语义）；处理类规格**没有规则**（动作链才是内容），
    /// 回读校验不能拿「规则非空」去卡它
    pub judge_class: bool,
    /// 迁移说明（哪些源内容没被带过来、为什么）—— 写进报告，也在这里留痕
    pub notes: Vec<String>,
}

/// 一次迁移的结果（HTTP 直接回这个结构，人读机读都在里面）。
#[derive(Debug, Clone)]
pub struct SeedReport {
    /// 本次新建的（幂等键 → specId）
    pub created: Vec<SeedOutcome>,
    /// 已存在、本次跳过的（不覆盖用户改动）
    pub existing: Vec<SeedOutcome>,
    pub verification: Verification,
}

#[derive(Debug, Clone)]
pub struct SeedOutcome {
    pub spec_id: i64,
    pub origin_key: String,
    pub name: String,
    pub plugin_id: String,
    pub rules: usize,
    pub thresholds: Value,
    pub params_diff: Value,
    pub notes: Vec<String>,
}

/// 回读校验结果：**全部为 true 才算迁移成功**。
#[derive(Debug, Clone)]
pub struct Verification {
    pub specs_total: usize,
    pub origin_unique: bool,
    pub thresholds_match_source: bool,
    pub rules_non_empty: bool,
    pub names_non_empty: bool,
    /// 处理类方案的**动作链**回读一致（KI-39：处理方案的内容就是动作链，丢了 = 跑起来变判定）
    pub actions_match_source: bool,
    /// 逐条回读明细（排障时能指出是哪条不对）
    pub detail: Vec<String>,
}

impl Verification {
    pub fn ok(&self) -> bool {
        self.origin_unique
            && self.thresholds_match_source
            && self.rules_non_empty
            && self.names_non_empty
            && self.actions_match_source
    }
}

// ---------------------------------------------------------------------------
// 源数据 → 默认规格（全部从 `core_config` 的装载结果派生，不手抄数字）
// ---------------------------------------------------------------------------

/// 数值型参数 = 阈值。其余（`prompt` / `expect` / `options` / `model` / 数组…）不是阈值：
/// 它们是 VL 提问措辞与模型选择，插件自带同义实现，不属于「迁移必须保真」的范畴。
fn numeric_thresholds(params: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    // 入参已经是 JSON 形态（调用方把 YAML 的 `check.params` 序列化一次再进来）：
    // 宿主不必依赖 YAML 的映射类型，也不必手抄数字。整数字面量保持整数（`700` 不会变 `700.0`）。
    let Value::Object(m) = params else {
        return out;
    };
    for (key, v) in m.clone() {
        // 只认数值：`prompt`/`expect`/`options`/`model` 这类是 VL 措辞与模型选择，不是阈值
        if v.is_number() {
            out.insert(key, v);
        }
    }
    out
}

/// 规则级阈值键 → 插件平铺参数键的**迁移映射**。
///
/// 为什么映射写在这里而不是宿主运行时：插件侧 `_FLAT_THRESHOLD_PARAMS` 是**业务映射**
/// （宿主不许懂业务），而「旧 YAML 的 `R1_size.min` 对应新插件的 `shortSideMin`」是
/// **一次性数据迁移**的事实，只能出现在迁移代码里。键名以插件为准，值仍从 YAML 派生。
struct FlatMap {
    flat_key: &'static str,
    rule_id: &'static str,
    /// 插件 `resources/rules.json` 里该规则 `params` 的键名（包内声明面，宿主不改名）
    pack_key: &'static str,
}

/// 电商包（有声明 schema 的插件）：每一项都必须在源阈值里取到值，取不到 = 迁移报错。
const ECOMMERCE_FLAT: &[FlatMap] = &[
    FlatMap { flat_key: "shortSideMin", rule_id: "R1_size", pack_key: "short_side_min" },
    FlatMap { flat_key: "whiteMaxDev", rule_id: "R2_white_bg", pack_key: "max_dev" },
    FlatMap { flat_key: "subjectRatioMin", rule_id: "R4_subject_ratio", pack_key: "min" },
    FlatMap { flat_key: "subjectRatioMax", rule_id: "R4_subject_ratio", pack_key: "max" },
];

/// 从插件包资源派生规格（电商 / 房产两族）。
///
/// `plugin_dir`：插件包目录（含 `resources/rules.json`）。
/// `pack_label`：幂等键里记的来源名（保持 `ecommerce-taobao` / `realestate` 原值 ——
/// 老库已经按这个键迁过一次，改名会让同一份规范迁出第二条）。
/// `flat`：该能力是否有参数 schema 需要折平铺参数（**空 = 无 schema** → 只保留规则级阈值，
/// 此时「没折平铺参数」不是丢失，是设计：阈值原样在 `spec.thresholds` 里）。
fn from_plugin_resources(
    plugin_dir: &std::path::Path,
    pack_label: &str,
    plugin_id: &str,
    name: &str,
    industry: &str,
    flat: &[FlatMap],
) -> Result<SeedSpec, String> {
    // 判据权威 = 插件的 `resources/rules.json`（与插件运行期读的同一份文件）
    let path = plugin_dir.join("resources").join("rules.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("插件资源装载失败（{}）：{e}", path.display()))?;
    let data: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("插件资源不是合法 JSON（{}）：{e}", path.display()))?;
    let pack_id = pack_label;
    let mut rules = Vec::new();
    let mut thresholds = Map::new();
    let mut params_diff = Map::new();
    let mut notes = Vec::new();
    // 只在规则级生效（插件参数 schema 未声明为平铺参数）的阈值键 —— 落 notes，不当错误
    let mut rule_level_only: Vec<String> = Vec::new();

    let Some(declared) = data.get("rules").and_then(|r| r.as_array()) else {
        return Err(format!(
            "插件资源（{}）没有 rules[]：无法派生默认规格",
            path.display()
        ));
    };
    for rule in declared {
        let Some(rule_id) = rule.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        rules.push(rule_id.to_string());
        // 规则参数（`params`）就是阈值；没有 params = 打标型/无阈值规则
        let nums = numeric_thresholds(rule.get("params").unwrap_or(&Value::Null));
        if nums.is_empty() {
            continue;
        }
        // 阈值**全部**落 spec.thresholds（插件按 rules[].params 消费，一处不少 = 不丢）。
        // 只有「插件参数 schema 声明了平铺键」的那几个必须能折成平铺参数（否则用户在规格里
        // 改参数改不到）；其余键原样留在 thresholds（不是丢失，是插件没打算让用户逐键改）。
        if !flat.is_empty() {
            for key in nums.keys() {
                let mapped = flat.iter().any(|m| m.rule_id == rule_id && m.pack_key == key);
                if !mapped {
                    rule_level_only.push(format!("{rule_id}.{key}"));
                }
            }
        }
        thresholds.insert(rule_id.to_string(), Value::Object(nums));
    }

    // 包级阈值（`rules.json` 顶层 `thresholds`）：VL 类插件把阈值声明在包级、规则 `params` 为空
    // （realestate 就是这个形状）。规则级 params 为空时必须回落这一层，否则规格会**静默丢阈值**，
    // 判定改用插件默认值 —— 那正好是「改插件默认值会悄悄改掉规格判定」的坑。
    // 规则级 `params` 优先：它是插件运行期读的那一处，包级只是没有 params 时的声明位。
    if let Some(Value::Object(by_rule)) = data.get("thresholds") {
        let mut inherited: Vec<String> = Vec::new();
        for rule_id in &rules {
            if thresholds.contains_key(rule_id) {
                continue;
            }
            let nums = numeric_thresholds(by_rule.get(rule_id).unwrap_or(&Value::Null));
            if nums.is_empty() {
                continue;
            }
            if !flat.is_empty() {
                for key in nums.keys() {
                    let mapped = flat.iter().any(|m| m.rule_id == *rule_id && m.pack_key == key);
                    if !mapped {
                        rule_level_only.push(format!("{rule_id}.{key}"));
                    }
                }
            }
            inherited.push(rule_id.clone());
            thresholds.insert(rule_id.clone(), Value::Object(nums));
        }
        if !inherited.is_empty() {
            notes.push(format!(
                "包级阈值（resources/rules.json 顶层 thresholds）已按规则并入规格：{} —— \
                 VL 类插件的规则没有 params，阈值只在这一层声明",
                inherited.join(" / ")
            ));
        }
    }

    if !rule_level_only.is_empty() {
        notes.push(format!(
            "这些阈值只在规格的规则级生效（插件参数 schema 未声明平铺键，用户不能在规格里逐键改）：\
             {}",
            rule_level_only.join(" / ")
        ));
    }

    // 平铺参数：值一律从 thresholds 里取（唯一真相是包内声明值，不在此处重抄数字）
    for m in flat {
        let Some(v) = thresholds
            .get(m.rule_id)
            .and_then(|o| o.get(m.pack_key))
            .cloned()
        else {
            // 映射声明了但源里没有该阈值 → 迁移配置与源不同步，必须喊出来
            return Err(format!(
                "迁移映射 {} ← {} × {} 在源（{pack_id}）里找不到对应阈值",
                m.flat_key, m.rule_id, m.pack_key
            ));
        };
        params_diff.insert(m.flat_key.to_string(), v);
    }

    if thresholds.is_empty() {
        notes.push(format!(
            "插件 {plugin_id} 的 resources/rules.json 没声明规则阈值：本条规格不带阈值，\
             判定用插件自带默认值（阈值要改就在规格里显式给）"
        ));
    }
    if flat.is_empty() {
        notes.push(format!(
            "该能力没有代码插件参数 schema（{plugin_id}）：阈值只以规则级形态保留在 spec.thresholds，\
             不进 paramsDiff（插件 ruleset 自带同名阈值，待插件声明 schema 后再折平铺参数）"
        ));
    }
    notes.push(format!(
        "源包 {pack_id} 的规则 {} 条全部保留；非阈值参数（VL prompt/expect/options、模型名、打标 outputs）\
         未搬运 —— 它们由插件自带实现，不是「阈值」",
        rules.len()
    ));

    Ok(SeedSpec {
        origin_key: format!("legacy-pack:{pack_id}"),
        name: name.to_string(),
        plugin_id: plugin_id.to_string(),
        industry: Some(industry.to_string()),
        source_pack: Some(pack_id.to_string()),
        rules,
        thresholds: Value::Object(thresholds),
        params_diff: Value::Object(params_diff),
        actions: json!([]),
        judge_class: true,
        notes,
    })
}

/// 三条默认规格（顺序 = 用户心智顺序：淘宝、Amazon、房源）。
///
/// **平台差异 = 规格**（N9）：Amazon 与淘宝共用同一个合规检测插件、同一套规则 id，
/// 区别只在规格里的阈值/启用集。Amazon 的数值取自计划卡（N9 面）——
/// A1 短边 ≥500 / A2 长边 ≥1600 / A3 白底偏差 ≤3 / A4 主体占比 0.55~0.85；
/// 这些数**写死在规格里**（不靠插件默认值兜底），否则改插件默认值会悄悄改掉 Amazon 判定。
fn default_seeds(conn: &Connection, data_dir: &std::path::Path) -> Result<Vec<SeedSpec>, String> {
    // 默认规格只能从**已装处理插件**的 resources 派生（IP-K16 起没有配置包可读）。
    // 电商插件没装 → 没有可迁的检查类默认规格（空列表，不是错误：界面会引导去装插件）；
    // 房产插件本来就要按行业场景装，同理。**处理类默认方案与它们无关**（各自按包派生），
    // 所以下面不用早退，两条腿各自判断。
    let mut seeds: Vec<SeedSpec> = Vec::new();
    if let Some(eco_dir) = crate::compliance_api::plugin_pack_dir_for(
        conn,
        data_dir,
        crate::compliance_api::PLUGIN_ECOMMERCE_ID,
    ) {
        let taobao = from_plugin_resources(
            &eco_dir,
            "ecommerce-taobao",
            crate::compliance_api::PLUGIN_ECOMMERCE_ID,
            "淘宝主图",
            "ecommerce",
            ECOMMERCE_FLAT,
        )?;
        // Amazon 规格与淘宝共用同一个插件：规则集相同，阈值按计划卡显式落规格
        // （淘宝档 700 短边/8 偏差/0.65 下限；Amazon 档 500 短边 + 1600 长边/3 偏差/0.55 下限）
        let amazon = SeedSpec {
            origin_key: "seed:ecommerce-amazon".to_string(),
            name: "亚马逊主图".to_string(),
            plugin_id: crate::compliance_api::PLUGIN_ECOMMERCE_ID.to_string(),
            industry: Some("ecommerce".to_string()),
            source_pack: None,
            rules: taobao.rules.clone(),
            thresholds: json!({
                "R1_size": {"short_side_min": 500, "long_side_min": 1600},
                "R2_white_bg": {"max_dev": 3},
                "R4_subject_ratio": {"min": 0.55, "max": 0.85},
            }),
            params_diff: json!({
                "shortSideMin": 500,
                "longSideMin": 1600,
                "whiteMaxDev": 3,
                "subjectRatioMin": 0.55,
                "subjectRatioMax": 0.85,
            }),
            actions: json!([]),
            judge_class: true,
            notes: vec![
                "Amazon 从来没有独立的旧行业包（仓内只有 ecommerce-taobao / realestate 的 YAML）→ \
                 本规格按 N9 计划卡给平台档显式阈值：A1 短边 ≥500 / A2 长边 ≥1600 / \
                 A3 白底偏差 ≤3 / A4 主体占比 0.55~0.85（阈值写死在规格里，不吃插件默认值）"
                    .to_string(),
                "规则集与淘宝规格同一套（规则 id 恒等，reasonCode 也就是同一个 id）：\
                 换平台只换阈值与启用集，不换插件、不改 ruleId"
                    .to_string(),
            ],
        };
        seeds.push(taobao);
        seeds.push(amazon);
        // 房产规格挂**代码插件身份**（`com.soujpg.pack.realestate`）：插件通道按规格的
        // pluginId 解析包目录（KI-31），写别的 id 都开不动房产包。
        if let Some(re_dir) =
            crate::compliance_api::plugin_pack_dir_for(conn, data_dir, PLUGIN_REALESTATE_ID)
        {
            seeds.push(from_plugin_resources(
                &re_dir,
                "realestate",
                PLUGIN_REALESTATE_ID,
                "房源照片",
                "realestate",
                &[],
            )?);
        }
    }
    // KI-39：处理类插件各自派生一条「官方默认方案」—— 否则能力面板里「官方默认」恒空，
    // 用户只能自建一条（界面建方案不编动作链 → 那条方案跑起来是判定语义，见 KI-39 现象 2）。
    for (plugin_id, dir) in installed_process_packs(conn, data_dir) {
        if let Some(seed) = process_seed_from_pack(&dir, &plugin_id)? {
            seeds.push(seed);
        }
    }
    Ok(seeds)
}

// ---------------------------------------------------------------------------
// KI-39（t_68362487）：处理类插件的「官方默认方案」
// ---------------------------------------------------------------------------

/// 处理类插件给宿主读的**动作清单**（`resources/actions.json`）。
///
/// 为什么读资源文件、不读 `pack.info.actions[]`：读 `pack.info` 要拉起插件子进程（网关 +
/// 握手 + 可能加载模型），而 `default_seeds` 挂在 `GET /api/specs` 的惰性补齐上 —— 读接口
/// 必须廉价。插件启动时也读这同一份文件生成 `pack.info.actions[]`，所以「一处声明两处消费」，
/// 不存在两份清单各自漂移（段包 `tests/test_action.py` 钉死这个等式）。
///
/// 没有这份文件 = 该包没声明宿主可读的动作（老包/纯参数面插件）→ `None`（不派生，不是错误）。
fn declared_actions(pack_dir: &std::path::Path) -> Result<Option<Vec<Value>>, String> {
    let path = pack_dir.join("resources").join("actions.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("动作清单读取失败（{}）：{e}", path.display())),
    };
    let data: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("动作清单不是合法 JSON（{}）：{e}", path.display()))?;
    let Some(list) = data.get("actions").and_then(|a| a.as_array()) else {
        return Err(format!("动作清单（{}）没有 actions[]", path.display()));
    };
    Ok(Some(list.clone()))
}

/// 处理类插件的「官方默认方案」：`actions = [{action: <包内声明的那个动作>}]`。
///
/// **只给恰好声明一个动作的包派生**（用户的裁决口径，KI-39 修法 B）：
/// * 一个动作 → 方案没有歧义（段包 1.2.x 的 `white_bg_fill` = 抠图白底）；
/// * 多个动作（imaging 六个）→ 派生哪一条都是**猜**，猜错的方案比没有方案更糟（用户以为
///   自己在「压缩」，实际跑的是「缩放」）→ 不派生，留给用户在方案里自建（界面动作链编辑另卡）；
/// * 一个都没声明 → 同上。
///
/// 阈值/参数一律不写死：处理类方案的参数走包内 `params.schema.json` 的默认值，规格只存 diff
/// （本方案 diff 为空 = 全用官方默认，用户改了才落库）。
fn process_seed_from_pack(
    pack_dir: &std::path::Path,
    plugin_id: &str,
) -> Result<Option<SeedSpec>, String> {
    let Some(declared) = declared_actions(pack_dir)? else {
        return Ok(None);
    };
    if declared.len() != 1 {
        return Ok(None);
    }
    let action = &declared[0];
    let Some(action_id) = action
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(format!(
            "处理插件 {plugin_id} 的动作清单里第一条没有 id（{}）",
            pack_dir.display()
        ));
    };
    // 方案名 = 动作自己的中文名（包里声明的「抠图白底」），缺了才回落插件短名
    let name = action
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            pack_host::Manifest::from_path(&pack_dir.join("manifest.json"))
                .map(|m| m.short_label("zh-Hans").to_string()) // i18n-F：种子落库的是持久默认值，固定中文
                .unwrap_or_else(|_| plugin_id.to_string())
        });
    let mut notes = vec![
        format!(
            "本方案是一条**处理**方案：动作链 = [{action_id}]（包内 resources/actions.json 声明的唯一动作），\
             参数全用包内 schema 默认值（规格只存你改过的那几项）"
        ),
        format!(
            "为什么只派生这一条：{plugin_id} 恰好声明一个动作，没有第二种理解；\
             多动作的包派生默认方案等于替用户猜动作，不派生"
        ),
    ];
    if let Some(summary) = action.get("summary").and_then(|v| v.as_str()) {
        if !summary.trim().is_empty() {
            notes.push(format!("包内对该动作的自述：{}", summary.trim()));
        }
    }
    Ok(Some(SeedSpec {
        origin_key: format!("seed:{plugin_id}"),
        name,
        plugin_id: plugin_id.to_string(),
        industry: None,
        source_pack: None,
        rules: Vec::new(),
        thresholds: json!({}),
        params_diff: json!({}),
        actions: json!([{ "action": action_id }]),
        judge_class: false,
        notes,
    }))
}

/// 已装的**纯处理类**插件（id + 包目录）：账本 current 行 ∪ `{data_dir}/packs/<id>` ∪ 仓库内置。
///
/// 三处来源与 `plugin_pack_dir_for` 的四级解析同口径（少了 `DAM_PACK_DIR` 强制目录：那是
/// 单个包的排障开关，不代表「装了哪些包」）。按 manifest id 去重，先到先得。
fn installed_process_packs(
    conn: &Connection,
    data_dir: &std::path::Path,
) -> Vec<(String, std::path::PathBuf)> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    // 1) 账本 current 行（IP-K08 安装形态：outputs 是安装时的事实，不现读 manifest）
    use core_storage::packs::PackRepo as _;
    if let Ok(rows) = core_storage::packs::SqlitePackRepo::new(conn).list() {
        for rec in rows.into_iter().filter(|r| r.current) {
            let outputs: Vec<&str> = rec.outputs.split(',').map(str::trim).collect();
            if outputs.contains(&"artifact") && !outputs.contains(&"verdict") {
                dirs.push(std::path::PathBuf::from(&rec.dir));
            }
        }
    }
    // 2) `{data_dir}/packs/<id>`（K05 扁平安装位）+ 3) 仓库内置 `packs/plugins/*`（容器/开发态）
    push_pack_dirs(&mut dirs, &data_dir.join(crate::compliance_api::PACKS_DIR));
    if let Some(root) = crate::compliance_api::repo_builtin_packs_root() {
        push_pack_dirs(&mut dirs, &root);
    }
    let mut out: Vec<(String, std::path::PathBuf)> = Vec::new();
    for dir in dirs {
        // 先做 v1.x 存量迁移（幂等）再解析 —— 已装的旧包升级后仍能派生方案
        if crate::compliance_api::load_pack_dir(&dir).is_err() {
            continue; // 半装的目录/坏 manifest：不是「装了处理插件」，跳过（装载路径会响亮报错）
        }
        let Ok(manifest) = pack_host::Manifest::from_path(&dir.join("manifest.json")) else {
            continue;
        };
        let outputs = manifest.effective_outputs();
        let process_only = outputs.iter().any(|k| k.as_str() == "artifact")
            && !outputs.iter().any(|k| k.as_str() == "verdict");
        if !process_only || !dir.join("resources").join("actions.json").is_file() {
            continue; // 纯判定包不归这条腿；没有动作清单的包派生不出方案
        }
        if out.iter().any(|(id, _)| id == &manifest.id) {
            continue;
        }
        out.push((manifest.id.clone(), dir));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 目录下每个 `*/manifest.json` 的父目录（缺失/读不了 = 跳过，不报错）。
fn push_pack_dirs(out: &mut Vec<std::path::PathBuf>, root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("manifest.json").is_file())
        .collect();
    dirs.sort(); // 目录序不确定：排序保证同一部署里派生顺序稳定
    out.extend(dirs);
}

// ---------------------------------------------------------------------------
// 幂等迁移 + 回读校验
// ---------------------------------------------------------------------------

fn outcome(rec: &SpecRecord, notes: Vec<String>) -> SeedOutcome {
    SeedOutcome {
        spec_id: rec.spec_id,
        origin_key: rec.origin_key.clone().unwrap_or_default(),
        name: rec.name.clone(),
        plugin_id: rec.plugin_id.clone(),
        rules: rec.rules.len(),
        thresholds: rec.thresholds.clone(),
        params_diff: rec.params_diff.clone(),
        notes,
    }
}

/// 幂等迁移：缺哪条补哪条，已存在的一律不覆盖。
///
/// 调用点两处（同一个函数，避免「启动跑一遍 / 接口跑一遍」两套语义）：
/// * `/api/specs` 读接口的惰性补齐（老库首次打开时补）；
/// * `POST /api/specs/migrate` 显式重跑（返回完整回读报告）。
pub fn ensure_legacy_specs(conn: &Connection, data_dir: &std::path::Path) -> Result<SeedReport, String> {
    let seeds = default_seeds(conn, data_dir)?;
    let repo = SqliteSpecRepo::new(conn);
    let mut created = Vec::new();
    let mut existing = Vec::new();
    for seed in &seeds {
        let new = core_storage::specs::NewSpec {
            plugin_id: seed.plugin_id.clone(),
            plugin_version_pin: None,
            name: seed.name.clone(),
            industry: seed.industry.clone(),
            rules: seed.rules.clone(),
            thresholds: seed.thresholds.clone(),
            actions: seed.actions.clone(),
            output: json!({}),
            params_diff: seed.params_diff.clone(),
            enabled: true,
            origin_key: Some(seed.origin_key.clone()),
        };
        let (spec_id, was_created) = repo
            .insert_if_absent(&new)
            .map_err(|e| format!("迁移规格「{}」失败：{e}", seed.name))?;
        let rec = repo
            .get(spec_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("迁移后回读不到规格 #{spec_id}"))?;
        if was_created {
            created.push(outcome(&rec, seed.notes.clone()));
        } else {
            let mut notes = seed.notes.clone();
            notes.push("已存在同来源规格：本次未覆盖（用户改过的值优先）".to_string());
            existing.push(outcome(&rec, notes));
        }
    }
    let created_ids: Vec<i64> = created.iter().map(|c| c.spec_id).collect();
    let verification = verify(conn, &seeds, &created_ids)?;
    let report = SeedReport {
        created,
        existing,
        verification,
    };
    if !report.verification.ok() {
        return Err(format!(
            "迁移回读校验未通过：{}",
            report.verification.detail.join("；")
        ));
    }
    Ok(report)
}

/// 回读校验：只信库里的行，不信内存里的期望值。
fn verify(conn: &Connection, seeds: &[SeedSpec], created_ids: &[i64]) -> Result<Verification, String> {
    let repo = SqliteSpecRepo::new(conn);
    let all = repo.list(None).map_err(|e| e.to_string())?;
    let mut detail = Vec::new();
    let mut origin_unique = true;
    let mut thresholds_match = true;
    let mut rules_non_empty = true;
    let mut names_non_empty = true;
    let mut actions_match = true;

    for seed in seeds {
        let rows: Vec<&SpecRecord> = all
            .iter()
            .filter(|r| r.origin_key.as_deref() == Some(seed.origin_key.as_str()))
            .collect();
        if rows.len() != 1 {
            origin_unique = false;
            detail.push(format!(
                "originKey {} 回读到 {} 行（应为 1）",
                seed.origin_key,
                rows.len()
            ));
            continue;
        }
        let row = rows[0];
        // 升级库（N18 实测）：老库可能有**同 originKey 的历史规格**（N1 之前的空壳、或用户
        // 改过的），它不归本轮种子所有、也没法重生（originKey 唯一索引）。对它做「阈值必须
        // 与源一致」的硬校验只会让 `/api/specs` 整个 500 → 界面看不到任何规格（正是本卡要
        // 消灭的「装了插件跑不了」）。这类行按现值保留，只在 detail 里留痕。
        // 只对「显然不是种子产物」的行放行：规则为空且阈值全空（N1 之前的空壳/老库遗留）。
        // 种子写出的行恒有规则；被存储吃掉阈值但规则还在的行仍然要响亮失败（下面的硬校验）。
        let empty_row = row.rules.is_empty()
            && matches!(&row.thresholds, Value::Object(m) if m.is_empty());
        if !created_ids.contains(&row.spec_id) && empty_row {
            detail.push(format!(
                "规格 #{}（{}）是既有行（originKey {}）：按现值保留，不做源值比对",
                row.spec_id, row.name, seed.origin_key
            ));
            continue;
        }
        if seed.judge_class && row.rules.is_empty() {
            rules_non_empty = false;
            detail.push(format!("规格 #{} 规则为空", row.spec_id));
        }
        if row.name.trim().is_empty() {
            names_non_empty = false;
            detail.push(format!("规格 #{} 名称为空", row.spec_id));
        }
        // 处理类方案：内容就是动作链（没有规则/阈值），回读必须能看见**同一个动作**
        if !seed.judge_class {
            let have = action_names(&row.actions);
            let want = action_names(&seed.actions);
            if have != want {
                actions_match = false;
                detail.push(format!(
                    "规格 #{} 动作链回读不一致（库里 {have:?}，源 {want:?}）",
                    row.spec_id
                ));
            }
        }
        // 阈值保真：源里每个数值键都必须还在。
        //
        // 只校验**未被改动过**的行（`createdAt == updatedAt` = 种子写入后没人动过）：
        // 用户合法收紧/删除阈值是他的自由，回读校验不该把用户编辑当迁移事故；
        // 反过来，没人动过却少了键 = 迁移/存储把数据吃了，必须响亮报错。
        let untouched = row.created_at == row.updated_at;
        if !untouched {
            detail.push(format!(
                "规格 #{}（{}）已被用户编辑过：阈值按现值保留，不做源值比对",
                row.spec_id, row.name
            ));
            continue;
        }
        if let Value::Object(src) = &seed.thresholds {
            for (rule, params) in src {
                let Some(Value::Object(have)) = row.thresholds.get(rule) else {
                    thresholds_match = false;
                    detail.push(format!(
                        "规格 #{} 丢了规则 {} 的阈值（源 {params:?}）",
                        row.spec_id, rule
                    ));
                    continue;
                };
                if let Value::Object(src_params) = params {
                    for key in src_params.keys() {
                        if !have.contains_key(key) {
                            thresholds_match = false;
                            detail.push(format!(
                                "规格 #{} 丢了阈值 {rule}.{key}",
                                row.spec_id
                            ));
                        }
                    }
                }
            }
        }
    }

    Ok(Verification {
        specs_total: all.len(),
        origin_unique,
        thresholds_match_source: thresholds_match,
        rules_non_empty,
        names_non_empty,
        actions_match_source: actions_match,
        detail,
    })
}

/// 动作链里的动作名（按声明顺序；形状不对 = 空）。只读接口/回读校验共用这一处口径。
fn action_names(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("action").and_then(|v| v.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 默认规格的**来源清单**（`/api/specs/migrate?dryRun=true` 与报告用：不写库也能看见会长什么样）。
pub fn planned_seeds(conn: &Connection, data_dir: &std::path::Path) -> Result<Vec<SeedSpec>, String> {
    default_seeds(conn, data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    /// 插件包目录（cargo test 下 = 仓库内置路径；与生产装载同一份）。
    fn pack_dir(rel: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packs/plugins")
            .join(rel)
    }

    /// 测试用 data_dir：插件目录解析的第 4 级是仓库内置路径，所以这里不需要真装包。
    fn data_dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent-dam-data-for-test")
    }

    /// 独立派生的「源阈值」：直接从插件 `resources/rules.json` 挑数值参数。
    ///
    /// 刻意不复用 `from_plugin_resources` 的实现（否则测的是「自己等于自己」）：
    /// 这里只认包资源事实 —— 规则 id + 数值参数 —— 用来钉「迁移不丢阈值」。
    fn source_thresholds(rel: &str) -> serde_json::Value {
        let raw = std::fs::read_to_string(pack_dir(rel).join("resources").join("rules.json")).unwrap();
        let data: Value = serde_json::from_str(&raw).unwrap();
        let mut out = Map::new();
        for rule in data["rules"].as_array().unwrap() {
            let Value::Object(m) = rule.get("params").cloned().unwrap_or(Value::Null) else {
                continue;
            };
            let nums: Map<String, Value> = m.into_iter().filter(|(_, v)| v.is_number()).collect();
            if !nums.is_empty() {
                out.insert(rule["id"].as_str().unwrap().to_string(), Value::Object(nums));
            }
        }
        Value::Object(out)
    }

    #[test]
    fn taobao_thresholds_match_pack_resources_exactly() {
        let conn = db();
        let seeds = default_seeds(&conn, &data_dir()).unwrap();
        let taobao = seeds
            .iter()
            .find(|s| s.origin_key == "legacy-pack:ecommerce-taobao")
            .expect("必须有淘宝主图规格");
        assert_eq!(
            taobao.thresholds,
            source_thresholds("ecommerce"),
            "淘宝规格的阈值必须与插件 resources 逐键逐值一致（禁止静默丢）"
        );
        // 平铺参数由同一份阈值折出来（不是另抄一份数字）
        assert_eq!(
            taobao.params_diff["shortSideMin"],
            taobao.thresholds["R1_size"]["short_side_min"]
        );
        assert_eq!(
            taobao.params_diff["whiteMaxDev"],
            taobao.thresholds["R2_white_bg"]["max_dev"]
        );
        assert_eq!(
            taobao.params_diff["subjectRatioMin"],
            taobao.thresholds["R4_subject_ratio"]["min"]
        );
        assert_eq!(
            taobao.params_diff["subjectRatioMax"],
            taobao.thresholds["R4_subject_ratio"]["max"]
        );
        // 规则一条不少
        assert_eq!(
            taobao.rules,
            vec![
                "R0_product_main",
                "R1_size",
                "R2_white_bg",
                "R4_subject_ratio",
                "R3_no_watermark",
                "R5_attribute"
            ]
        );
    }

    #[test]
    fn realestate_spec_inherits_pack_level_thresholds() {
        let conn = db();
        let seeds = default_seeds(&conn, &data_dir()).unwrap();
        let re = seeds
            .iter()
            .find(|s| s.origin_key == "legacy-pack:realestate")
            .expect("必须有房源照片规格");
        // 房产包的阈值声明在 rules.json **顶层 thresholds**（规则本身没有 params）→
        // 必须按规则级并进规格：旧 YAML 时代的 100 / 0.3 / 8 一个都不能丢
        assert_eq!(re.thresholds["F1_sharpness"]["min"], serde_json::json!(100.0));
        assert_eq!(
            re.thresholds["F2_exposure"]["max_dead_ratio"],
            serde_json::json!(0.3)
        );
        assert_eq!(re.thresholds["F3_duplicate"]["threshold"], serde_json::json!(8));
        assert_eq!(
            re.thresholds["F5_min_size"]["short_side_min"],
            serde_json::json!(600)
        );
        // 房产包没在 resources 声明参数 schema → 不许凭空造 paramsDiff
        assert_eq!(re.params_diff, serde_json::json!({}));
        assert_eq!(re.rules.len(), 6);
        assert!(
            re.notes.iter().any(|n| n.contains("包级阈值")),
            "必须写明阈值来自包级声明：{:?}",
            re.notes
        );
    }

    #[test]
    fn amazon_seed_carries_platform_thresholds_explicitly() {
        let conn = db();
        let seeds = default_seeds(&conn, &data_dir()).unwrap();
        let amz = seeds
            .iter()
            .find(|s| s.origin_key == "seed:ecommerce-amazon")
            .expect("必须有亚马逊主图规格");
        // N9：平台差异 = 规格。Amazon 阈值写死在规格里（A1 500 / A2 1600 / A3 3 / A4 0.55~0.85），
        // 不吃插件默认值 —— 否则改插件默认值会悄悄改掉 Amazon 判定。
        assert_eq!(amz.thresholds["R1_size"]["short_side_min"], serde_json::json!(500));
        assert_eq!(amz.thresholds["R1_size"]["long_side_min"], serde_json::json!(1600));
        assert_eq!(amz.thresholds["R2_white_bg"]["max_dev"], serde_json::json!(3));
        assert_eq!(amz.thresholds["R4_subject_ratio"]["min"], serde_json::json!(0.55));
        assert_eq!(amz.thresholds["R4_subject_ratio"]["max"], serde_json::json!(0.85));
        // 平铺参数与阈值同源（插件 schema 键），宿主合并后就是全量生效值
        assert_eq!(amz.params_diff["shortSideMin"], serde_json::json!(500));
        assert_eq!(amz.params_diff["longSideMin"], serde_json::json!(1600));
        assert_eq!(amz.params_diff["whiteMaxDev"], serde_json::json!(3));
        assert!(!amz.notes.is_empty(), "必须写明阈值口径出处");
    }

    #[test]
    fn amazon_and_taobao_share_plugin_and_rule_ids() {
        // 「多条规格不是多个插件」：电商两条规格挂同一个插件 id、规则 id 逐条相同，
        // 差异只在阈值（reasonCode = ruleId，所以跨平台对账用同一个机读码）。
        let conn = db();
        let seeds = default_seeds(&conn, &data_dir()).unwrap();
        let taobao = seeds
            .iter()
            .find(|s| s.origin_key == "legacy-pack:ecommerce-taobao")
            .unwrap();
        let amz = seeds
            .iter()
            .find(|s| s.origin_key == "seed:ecommerce-amazon")
            .unwrap();
        assert_eq!(amz.plugin_id, taobao.plugin_id);
        assert_eq!(amz.rules, taobao.rules);
        assert_eq!(amz.plugin_id, crate::compliance_api::PLUGIN_ECOMMERCE_ID);
        // 阈值确实不同（否则「两个平台」只是换个名字）
        assert_ne!(
            amz.thresholds["R1_size"]["short_side_min"],
            taobao.thresholds["R1_size"]["short_side_min"]
        );
        assert_ne!(
            amz.thresholds["R2_white_bg"]["max_dev"],
            taobao.thresholds["R2_white_bg"]["max_dev"]
        );
    }

    #[test]
    fn ensure_is_idempotent_and_verifies_readback() {
        let conn = db();
        let first = ensure_legacy_specs(&conn, &data_dir()).unwrap();
        // 3 条检查类（淘宝/Amazon/房源）+ 1 条处理类官方默认方案（段包：唯一动作 white_bg_fill）
        let expected = default_seeds(&conn, &data_dir()).unwrap().len();
        assert_eq!(first.created.len(), expected);
        assert!(first.created.iter().all(|c| c.spec_id > 0));
        assert!(first.verification.ok(), "{:?}", first.verification.detail);
        assert_eq!(first.verification.specs_total, expected);

        // 第二遍：不新建（幂等），回读仍通过
        let second = ensure_legacy_specs(&conn, &data_dir()).unwrap();
        assert!(second.created.is_empty(), "幂等：第二遍不许新建");
        assert_eq!(second.existing.len(), expected);
        assert!(second.verification.ok());

        // 用户改过阈值后重跑：不覆盖（迁移是补齐，不是重置）
        use core_storage::specs::{SpecPatch, SpecRepo, SqliteSpecRepo};
        let repo = SqliteSpecRepo::new(&conn);
        let taobao = repo
            .find_by_origin("legacy-pack:ecommerce-taobao")
            .unwrap()
            .unwrap();
        repo.update(
            taobao.spec_id,
            &SpecPatch {
                thresholds: Some(serde_json::json!({"R1_size": {"min": 1000}})),
                ..Default::default()
            },
        )
        .unwrap();
        let third = ensure_legacy_specs(&conn, &data_dir()).unwrap();
        assert!(third.created.is_empty());
        let after = repo.get(taobao.spec_id).unwrap().unwrap();
        assert_eq!(
            after.thresholds,
            serde_json::json!({"R1_size": {"min": 1000}}),
            "重跑迁移不许覆盖用户改过的阈值"
        );
    }

    #[test]
    fn verification_catches_missing_threshold_key() {
        let conn = db();
        ensure_legacy_specs(&conn, &data_dir()).unwrap();
        // 模拟「存储层把阈值吃了」：直接改 raw 列，**不动 updatedAt**
        // （不动时间戳 = 这条规格在账面上仍是「种子原样」，回读必须发现它跟源不一致）
        let taobao: i64 = conn
            .query_row(
                "SELECT specId FROM plugin_spec WHERE originKey = 'legacy-pack:ecommerce-taobao'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE plugin_spec SET thresholds = '{}' WHERE specId = ?1",
            rusqlite::params![taobao],
        )
        .unwrap();
        let err = ensure_legacy_specs(&conn, &data_dir()).unwrap_err();
        assert!(err.contains("丢了"), "回读校验必须指出丢的是哪条：{err}");
    }

    // -----------------------------------------------------------------------
    // KI-39（t_68362487）：处理类插件的官方默认方案
    // -----------------------------------------------------------------------

    /// 段包（恰好声明一个动作）必须有官方默认方案：面板「官方默认」分组不空、且方案带动作链。
    #[test]
    fn single_action_process_pack_gets_official_default_plan() {
        let conn = db();
        let seeds = default_seeds(&conn, &data_dir()).unwrap();
        let seg = seeds
            .iter()
            .find(|s| s.origin_key == "seed:com.soujpg.pack.segment")
            .expect("段包必须派生官方默认方案（KI-39：否则能力面板「官方默认」恒空、发不动）");
        // 动作/名字都来自**包资源**（不是宿主写死）
        let declared = declared_actions(&pack_dir("segment"))
            .unwrap()
            .expect("段包必须声明 resources/actions.json");
        assert_eq!(declared.len(), 1);
        assert_eq!(seg.actions, serde_json::json!([{ "action": declared[0]["id"] }]));
        assert_eq!(seg.name, declared[0]["name"]);
        assert_eq!(seg.plugin_id, declared_actions_plugin_id());
        // 处理类方案没有规则/阈值：参数全用包内 schema 默认值（规格只存 diff）
        assert!(!seg.judge_class);
        assert!(seg.rules.is_empty());
        assert_eq!(seg.thresholds, serde_json::json!({}));
        assert_eq!(seg.params_diff, serde_json::json!({}));
        assert_eq!(seg.source_pack, None);
        assert!(!seg.notes.is_empty(), "必须写明方案来源与为何只派生这一条");
    }

    /// 段包的 manifest id（独立读文件，不借 `default_seeds` 的实现）。
    fn declared_actions_plugin_id() -> String {
        let raw = std::fs::read_to_string(pack_dir("segment").join("manifest.json")).unwrap();
        serde_json::from_str::<Value>(&raw).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// 多动作包/没清单的包都不派生：派生哪一条都是替用户猜动作。
    #[test]
    fn ambiguous_process_pack_gets_no_default_plan() {
        let multi = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(multi.path().join("resources")).unwrap();
        std::fs::write(
            multi.path().join("resources").join("actions.json"),
            r#"{"actions":[{"id":"resize","name":"缩放"},{"id":"compress","name":"压缩"}]}"#,
        )
        .unwrap();
        assert!(
            process_seed_from_pack(multi.path(), "com.soujpg.pack.imaging")
                .unwrap()
                .is_none(),
            "六个/两个动作的包派生默认方案 = 替用户猜动作"
        );
        // 没有 actions.json = 该包没声明宿主可读的动作 → 不派生（不是错误）
        let bare = tempfile::tempdir().unwrap();
        assert!(process_seed_from_pack(bare.path(), "any.pack").unwrap().is_none());
        // 坏清单 = 响亮失败（坏包不许静默变成一个「没有方案」）
        let broken = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(broken.path().join("resources")).unwrap();
        std::fs::write(broken.path().join("resources").join("actions.json"), "{not json").unwrap();
        assert!(process_seed_from_pack(broken.path(), "any.pack").is_err());
    }

    /// 处理类默认方案落库后：动作链在库里原样读得回来（回读校验的 actionsMatchSource 不是摆设）。
    #[test]
    fn process_default_spec_keeps_action_chain_in_db() {
        let conn = db();
        let report = ensure_legacy_specs(&conn, &data_dir()).unwrap();
        assert!(
            report.verification.actions_match_source,
            "{:?}",
            report.verification.detail
        );
        use core_storage::specs::SpecRepo;
        let row = SqliteSpecRepo::new(&conn)
            .find_by_origin("seed:com.soujpg.pack.segment")
            .unwrap()
            .expect("段包默认方案必须落库");
        assert_eq!(action_names(&row.actions), vec!["white_bg_fill".to_string()]);
        assert!(row.enabled, "默认方案必须可直接执行");
    }
}
