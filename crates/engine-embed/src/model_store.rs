//! 模型分发：manifest（URL + sha256 锁定）+ 落盘状态 + 下载/校验（PLAN.md §9「首启静默下载」）。
//!
//! 目录布局：`{appData}/models/<spec.name>/{vision.onnx,text.onnx,tokenizer.json}`。
//! 单文件 URL = `base_url` + "/" + `file.url_path`；`base_url` 支持 `file://`（离线/内网/测试分发）。
//! 下载先落 `.part`，边下边算 sha256，大小与哈希双校验通过才 rename 成正式名 ——
//! 半截文件永不进模型目录（可重入续下）。
//!
//! 类型用 owned String/vec（而非 const 静态）：manifest 需可注入/可测（假模型、内网镜像）。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{EmbedError, Result};

/// 单个模型文件声明（sha256 锁定：变更 = 重下载，与 preprocess 同级）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelFile {
    /// 落盘文件名（provider 按此名寻址）
    pub name: String,
    /// 分发源相对路径（URL = base_url + "/" + url_path）
    pub url_path: String,
    /// 期望字节数（快速存在性校验）
    pub bytes: u64,
    /// 期望 sha256（下载后强校验）
    pub sha256: String,
}

/// 模型 manifest。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelManifest {
    pub name: String,
    pub files: Vec<ModelFile>,
    /// 文本塔最大 token 数（SigLIP2 = 64，超长截断）
    pub text_max_length: usize,
}

impl ModelManifest {
    pub fn file(&self, name: &str) -> Option<&ModelFile> {
        self.files.iter().find(|f| f.name == name)
    }
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }
}

/// SigLIP 2 base patch16-224（双塔 fp16 ONNX + Gemma tokenizer）——默认内嵌 embedding。
/// 源：onnx-community/siglip2-base-patch16-224-ONNX（vision_model_fp16 / text_model_fp16）。
pub fn siglip2_base_p224() -> ModelManifest {
    ModelManifest {
        name: core_config::DEFAULT_EMBEDDER.to_string(),
        text_max_length: 64,
        files: vec![
            ModelFile {
                name: "vision.onnx".into(),
                url_path: "onnx/vision_model_fp16.onnx".into(),
                bytes: 186_039_516,
                sha256: "a1959f7bd3993a607e48839f6d01e25b876fe76afda301b028b78eef68aabd95".into(),
            },
            ModelFile {
                name: "text.onnx".into(),
                url_path: "onnx/text_model_fp16.onnx".into(),
                bytes: 564_862_230,
                sha256: "711da56ada0a4aa11c7dd3320df741081a3cae4f0ae1b5e5c6d5b294738d0eb0".into(),
            },
            ModelFile {
                name: "tokenizer.json".into(),
                url_path: "tokenizer.json".into(),
                bytes: 34_363_039,
                sha256: "cb9140fae3ac5122c972d37adf83e1248471a38147ad76f8215c8872c6fd8322".into(),
            },
        ],
    }
}

/// 已知模型 manifest 表（新增模型 = 加一条 + registry 加一条，零改核心）。
pub fn manifests() -> Vec<ModelManifest> {
    vec![
        siglip2_base_p224(),
        dinov3_vits16(),
        u2net(),
        realesr_general_x4v3(),
        lama_fp32(),
        ppocrv6_medium_det(),
        ppocrv6_medium_rec(),
    ]
}

/// PP-OCRv6_medium_det 文字检测（OCR-E；PaddlePaddle 官方 ONNX 仓，Apache-2.0，
/// 62,032,837 B）。分发源 HF 官方仓（url_path 相对 base_url，自建镜像/file:// 兜底）。
/// 消费方 = engine-ocr::DetSession（DB 后处理 thresh 0.2 / box_thresh 0.45 / unclip 1.4）。
pub fn ppocrv6_medium_det() -> ModelManifest {
    ModelManifest {
        name: "ppocrv6-medium-det".to_string(),
        text_max_length: 0, // image-only
        files: vec![ModelFile {
            name: "inference.onnx".into(),
            url_path: "ppocrv6-medium-det/inference.onnx".into(),
            bytes: 62_032_837,
            sha256: "eb13b44b25bb36f89528b68720af8a61d9cf381176107f465db1757b65d086e1".into(),
        }],
    }
}

/// PP-OCRv6_medium_rec 文字识别（OCR-E；PaddlePaddle 官方 ONNX 仓，Apache-2.0）。
/// 两件：inference.onnx（76,554,979 B）+ ppocrv6_dict.txt（官方 inference.yml
/// character_dict 落盘 74,947 B，18,708 字符；CTC blank=0 space=末位，类别 18710）。
/// 消费方 = engine-ocr::RecSession。
pub fn ppocrv6_medium_rec() -> ModelManifest {
    ModelManifest {
        name: "ppocrv6-medium-rec".to_string(),
        text_max_length: 0, // image-only
        files: vec![
            ModelFile {
                name: "inference.onnx".into(),
                url_path: "ppocrv6-medium-rec/inference.onnx".into(),
                bytes: 76_554_979,
                sha256: "9c09abf0957f7968c7586464b7397b84ad2387a0497a351af40e9acc71b673ba".into(),
            },
            ModelFile {
                name: "ppocrv6_dict.txt".into(),
                url_path: "ppocrv6-medium-rec/ppocrv6_dict.txt".into(),
                bytes: 74_947,
                sha256: "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d".into(),
            },
        ],
    }
}

/// u2net 主体分割（C3；rembg 同源 FP32，~168MB，单文件）。
/// 分发源：GitHub rembg release / HF 镜像 tomjackson2023/rembg（url_path 相对 base_url，
/// base_url 可指自建镜像或 file://）；桌面端随安装包内置分发（2026-10-16 用户裁决），
/// 公网下载仅 web 表单兜底。消费方 = engine-segment（fg_ratio/smart_crop）。
pub fn u2net() -> ModelManifest {
    ModelManifest {
        name: "u2net".to_string(),
        text_max_length: 0, // image-only
        files: vec![ModelFile {
            name: "u2net.onnx".into(),
            url_path: "u2net.onnx".into(),
            bytes: 175_997_641,
            sha256: "8d10d2f3bb75ae3b6d527c77944fc5e7dcd94b29809d47a739a7a728a912b491".into(),
        }],
    }
}

/// realesr-general-x4v3 超分（C4；Real-ESRGAN 官方权重 ONNX 再打包，~4.7MB 单文件，
/// BSD-3-Clause）。轻量档随安装包内置分发；url_path 相对 base_url（自建镜像/file:// 兜底）。
/// 消费方 = engine-restore::upscale（4× 放大）。
pub fn realesr_general_x4v3() -> ModelManifest {
    ModelManifest {
        name: "realesr-general-x4v3".to_string(),
        text_max_length: 0, // image-only
        files: vec![ModelFile {
            name: "realesr-general-x4v3.onnx".into(),
            url_path: "realesr-general-x4v3.onnx".into(),
            bytes: 4_866_417,
            sha256: "1940a93ee08283a0a7286183186357b1688fe9fa8ede74604b424586aaddf112".into(),
        }],
    }
}

/// big-lama 局部重绘（C4；Carve/LaMa-ONNX 导出 FP32，~208MB 单文件，Apache-2.0）。
/// 体积原因不随安装包内置——分发走 model_store（自建镜像 / file://）。
/// 消费方 = engine-restore::inpaint（512² 推理 + paste-back）。
pub fn lama_fp32() -> ModelManifest {
    ModelManifest {
        name: "lama-fp32".to_string(),
        text_max_length: 0, // image-only
        files: vec![ModelFile {
            name: "lama-fp32.onnx".into(),
            url_path: "lama-fp32.onnx".into(),
            bytes: 208_044_816,
            sha256: "1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6".into(),
        }],
    }
}

/// DINOv3 ViT-S/16 去重塔（2026-10-13 双嵌入器裁决；本地导出 ONNX + 外置权重）。
/// 公网分发源未定（hf-mirror 无公开仓，见 D5 报告）——url_path 为相对路径，
/// base_url 指自建镜像或 file:// 本地分发即可用。
pub fn dinov3_vits16() -> ModelManifest {
    ModelManifest {
        name: core_config::DEDUP_EMBEDDER.to_string(),
        text_max_length: 0, // image-only，无文本塔
        files: vec![
            ModelFile {
                name: "model.onnx".into(),
                url_path: "dinov3-vits16/model.onnx".into(),
                bytes: 137_969,
                sha256: "bb75e9e30ff382ecdbd150266445ab41272be4acc85fd3563218dd89781e36da".into(),
            },
            ModelFile {
                name: "model.onnx_data".into(),
                url_path: "dinov3-vits16/model.onnx_data".into(),
                bytes: 86_347_776,
                sha256: "1eff0bb9f4fdef831ca61c8bc2b5c88d8cc21ef4756d3b483d9b9c15d8d5d27f".into(),
            },
        ],
    }
}

/// 按 registry spec.name 找 manifest（下载/校验用）。
pub fn manifest_for(name: &str) -> Option<ModelManifest> {
    manifests().into_iter().find(|m| m.name == name)
}

/// 单文件实测状态。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileCheck {
    pub name: String,
    pub urlPath: String,
    pub present: bool,
    pub bytes: u64,
    pub expectedBytes: u64,
    pub expectedSha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shaOk: Option<bool>,
}

/// 下载/校验事件（上层转成 app 事件推前端）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DownloadEvent {
    /// downloading / verifying / skipped / done / failed
    pub status: String,
    pub model: String,
    pub file: String,
    pub doneBytes: u64,
    pub totalBytes: u64,
    pub completedFiles: usize,
    pub totalFiles: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 下载结果汇总。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DownloadOutcome {
    pub model: String,
    pub downloaded: usize,
    pub skipped: usize,
    pub totalBytes: u64,
}

/// 模型存储/分发器：定位模型目录 + 状态检查 + 下载（sha256 强校验）。
pub struct ModelStore {
    root: PathBuf,
    base_url: String,
    agent: ureq::Agent,
}

impl ModelStore {
    /// root = `{appData}/models`；base_url 为空时用 core-config 默认镜像。
    pub fn new(root: impl Into<PathBuf>, base_url: impl Into<String>) -> Self {
        let base_url = {
            let b = base_url.into();
            if b.trim().is_empty() {
                core_config::DEFAULT_MODEL_BASE_URL.to_string()
            } else {
                b.trim_end_matches('/').to_string()
            }
        };
        Self {
            root: root.into(),
            base_url,
            agent: super::http::agent(30 * 60),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 模型目录：`{root}/<manifest.name>`。
    pub fn dir(&self, m: &ModelManifest) -> PathBuf {
        self.root.join(&m.name)
    }

    pub fn file_path(&self, m: &ModelManifest, f: &ModelFile) -> PathBuf {
        self.dir(m).join(&f.name)
    }

    /// 文件存在且字节数吻合（快路径，不做 sha256）。
    pub fn status(&self, m: &ModelManifest) -> Vec<FileCheck> {
        m.files
            .iter()
            .map(|f| {
                let p = self.file_path(m, f);
                let bytes = std::fs::metadata(&p).map(|md| md.len()).unwrap_or(0);
                FileCheck {
                    name: f.name.clone(),
                    urlPath: f.url_path.clone(),
                    present: bytes == f.bytes,
                    bytes,
                    expectedBytes: f.bytes,
                    expectedSha256: f.sha256.clone(),
                    sha256: None,
                    shaOk: None,
                }
            })
            .collect()
    }

    /// 模型文件全齐（快路径，未做 sha 校验）。
    pub fn is_complete(&self, m: &ModelManifest) -> bool {
        self.status(m).iter().all(|c| c.present)
    }

    /// 全量 sha256 校验（流式读，不占内存）。
    pub fn verify(&self, m: &ModelManifest) -> Result<Vec<FileCheck>> {
        let mut out = Vec::new();
        for f in &m.files {
            let p = self.file_path(m, f);
            if !p.is_file() {
                out.push(FileCheck {
                    name: f.name.clone(),
                    urlPath: f.url_path.clone(),
                    present: false,
                    bytes: 0,
                    expectedBytes: f.bytes,
                    expectedSha256: f.sha256.clone(),
                    sha256: None,
                    shaOk: Some(false),
                });
                continue;
            }
            let (bytes, sha) = sha256_file(&p)?;
            out.push(FileCheck {
                name: f.name.clone(),
                urlPath: f.url_path.clone(),
                present: true,
                bytes,
                expectedBytes: f.bytes,
                expectedSha256: f.sha256.clone(),
                sha256: Some(sha.clone()),
                shaOk: Some(sha == f.sha256),
            });
        }
        Ok(out)
    }

    /// 下载缺失/损坏文件（force=true 时全部重下）。逐文件进度回调。
    /// 失败语义：单文件失败立即返回错误，已下好的文件保留（可重入续下）。
    pub fn download(
        &self,
        m: &ModelManifest,
        force: bool,
        progress: &mut dyn FnMut(DownloadEvent),
    ) -> Result<DownloadOutcome> {
        std::fs::create_dir_all(self.dir(m))?;
        let total_files = m.files.len();
        let total_bytes = m.total_bytes();
        let mut outcome = DownloadOutcome {
            model: m.name.clone(),
            ..Default::default()
        };
        let mut done_bytes: u64 = 0;
        let mut completed = 0usize;

        for f in &m.files {
            let dest = self.file_path(m, f);
            let ok_on_disk = dest.is_file()
                && std::fs::metadata(&dest).map(|md| md.len()).unwrap_or(0) == f.bytes;
            if ok_on_disk && !force {
                done_bytes += f.bytes;
                completed += 1;
                outcome.skipped += 1;
                progress(DownloadEvent {
                    status: "skipped".into(),
                    model: m.name.clone(),
                    file: f.name.clone(),
                    doneBytes: done_bytes,
                    totalBytes: total_bytes,
                    completedFiles: completed,
                    totalFiles: total_files,
                    error: None,
                });
                continue;
            }

            let mk = |status: &str, dbytes: u64, completed: usize, error: Option<String>| {
                DownloadEvent {
                    status: status.into(),
                    model: m.name.clone(),
                    file: f.name.clone(),
                    doneBytes: dbytes,
                    totalBytes: total_bytes,
                    completedFiles: completed,
                    totalFiles: total_files,
                    error,
                }
            };

            progress(mk("downloading", done_bytes, completed, None));
            let tmp = dest.with_extension("part");
            let mut last_reported = done_bytes;
            let fetched = self.fetch_to_file(&f.url_path, &tmp, &mut |file_done| {
                let absolute = done_bytes + file_done;
                if absolute != last_reported {
                    last_reported = absolute;
                    progress(mk("downloading", absolute, completed, None));
                }
            });
            match fetched {
                Ok((bytes, sha)) => {
                    if bytes != f.bytes || sha != f.sha256 {
                        let _ = std::fs::remove_file(&tmp);
                        let msg = format!(
                            "{}: size {bytes} (expect {}), sha256 {sha} (expect {})",
                            f.name, f.bytes, f.sha256
                        );
                        progress(mk("failed", done_bytes, completed, Some(msg.clone())));
                        return Err(EmbedError::ModelCorrupt(msg));
                    }
                    progress(mk("verifying", done_bytes, completed, None));
                    std::fs::rename(&tmp, &dest)?;
                    done_bytes += f.bytes;
                    completed += 1;
                    outcome.downloaded += 1;
                    outcome.totalBytes += f.bytes;
                    progress(mk("done", done_bytes, completed, None));
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    progress(mk("failed", done_bytes, completed, Some(e.to_string())));
                    return Err(e);
                }
            }
        }
        Ok(outcome)
    }

    /// 单个文件 → 本地文件，边写边算 sha256；progress 报该文件已写字节数。
    fn fetch_to_file(
        &self,
        url_path: &str,
        dest: &Path,
        progress: &mut dyn FnMut(u64),
    ) -> Result<(u64, String)> {
        let url = format!("{}/{}", self.base_url, url_path);
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        let mut last_report: u64 = 0;
        let mut out = std::fs::File::create(dest)?;

        let mut pump = |reader: &mut dyn Read,
                        out: &mut std::fs::File,
                        written: &mut u64,
                        hasher: &mut Sha256|
         -> Result<()> {
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])?;
                *written += n as u64;
                if *written - last_report >= 4 * 1024 * 1024 {
                    last_report = *written;
                    progress(*written);
                }
            }
            Ok(())
        };

        if let Some(path) = url.strip_prefix("file://") {
            let mut f = std::fs::File::open(path)
                .map_err(|e| EmbedError::Http(format!("file://{path}: {e}")))?;
            pump(&mut f, &mut out, &mut written, &mut hasher)?;
        } else {
            let resp = self
                .agent
                .get(&url)
                .call()
                .map_err(|e| EmbedError::Http(format!("GET {url}: {e}")))?;
            let mut reader = resp.into_body().into_reader();
            pump(&mut reader, &mut out, &mut written, &mut hasher)?;
        }
        out.flush()?;
        drop(out);
        progress(written);
        Ok((written, hex(&hasher.finalize())))
    }
}

/// 流式文件 sha256 → (字节数, 十六进制摘要)。
pub fn sha256_file(path: &Path) -> Result<(u64, String)> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex(&hasher.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dam-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 用真实文件内容造一个小 manifest（sha 由实测得到），用于离线 file:// 下载测试。
    fn fake_manifest(src: &Path, name: &str) -> ModelManifest {
        let mut files = Vec::new();
        for fname in ["a.onnx", "b.json"] {
            let p = src.join(fname);
            std::fs::write(&p, format!("content-of-{fname}")).unwrap();
            let (bytes, sha256) = sha256_file(&p).unwrap();
            files.push(ModelFile {
                name: fname.to_string(),
                url_path: fname.to_string(),
                bytes,
                sha256,
            });
        }
        ModelManifest {
            name: name.to_string(),
            files,
            text_max_length: 8,
        }
    }

    #[test]
    fn c4_restore_manifests_are_locked() {
        let x = realesr_general_x4v3();
        assert_eq!(x.name, "realesr-general-x4v3");
        assert_eq!(x.files.len(), 1);
        let f = x.file("realesr-general-x4v3.onnx").unwrap();
        assert_eq!(f.bytes, 4_866_417);
        assert_eq!(
            f.sha256,
            "1940a93ee08283a0a7286183186357b1688fe9fa8ede74604b424586aaddf112"
        );
        let l = lama_fp32();
        assert_eq!(l.name, "lama-fp32");
        let lf = l.file("lama-fp32.onnx").unwrap();
        assert_eq!(lf.bytes, 208_044_816);
        assert_eq!(
            lf.sha256,
            "1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6"
        );
        // manifests 表可寻址（下载管理页可见）
        assert!(manifest_for("realesr-general-x4v3").is_some());
        assert!(manifest_for("lama-fp32").is_some());
        // registry 与 manifest 同名可寻址（C4 两模型）
        let reg = core_config::load_registry(None).unwrap();
        assert!(reg.models.iter().any(|s| s.name == x.name));
        assert!(reg.models.iter().any(|s| s.name == l.name));
    }

    #[test]
    fn real_siglip2_manifest_is_locked() {
        let m = siglip2_base_p224();
        assert_eq!(m.name, core_config::DEFAULT_EMBEDDER);
        assert_eq!(m.text_max_length, 64);
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.file("vision.onnx").unwrap().bytes, 186_039_516);
        assert_eq!(m.file("text.onnx").unwrap().sha256.len(), 64);
        assert!(manifest_for("nope").is_none());
        assert_eq!(
            manifest_for(core_config::DEFAULT_EMBEDDER)
                .unwrap()
                .files
                .len(),
            3
        );
        // registry 里的 provider 必须与 manifest 同名可寻址
        let reg = core_config::load_registry(None).unwrap();
        assert!(reg.models.iter().any(|s| s.name == m.name));
    }

    #[test]
    fn store_paths_and_default_base_url() {
        let store = ModelStore::new("/tmp/x/models", "");
        assert!(store.base_url().starts_with("https://hf-mirror.com"));
        let store2 = ModelStore::new("/tmp/x/models", "http://127.0.0.1:8080/files/");
        assert_eq!(store2.base_url(), "http://127.0.0.1:8080/files");
        let m = siglip2_base_p224();
        assert_eq!(
            store.dir(&m),
            PathBuf::from("/tmp/x/models/siglip2-base-p224-onnx")
        );
        assert_eq!(
            store.file_path(&m, m.file("text.onnx").unwrap()),
            PathBuf::from("/tmp/x/models/siglip2-base-p224-onnx/text.onnx")
        );
    }

    #[test]
    fn download_from_file_url_with_sha_verify_then_skip() {
        let root = tmp_root("fileurl");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let m = fake_manifest(&src, "fake-model");
        let store = ModelStore::new(root.join("models"), format!("file://{}", src.display()));

        assert!(!store.is_complete(&m));
        let mut events = Vec::new();
        let outcome = store.download(&m, false, &mut |e| events.push(e)).unwrap();
        assert_eq!(outcome.downloaded, 2);
        assert_eq!(outcome.skipped, 0);
        assert!(outcome.totalBytes > 0);
        assert!(store.is_complete(&m));
        assert_eq!(events.first().unwrap().status, "downloading");
        assert_eq!(events.last().unwrap().status, "done");
        assert_eq!(
            events.last().unwrap().doneBytes,
            events.last().unwrap().totalBytes
        );
        // 全量 sha 校验通过（验收 2 的机制）
        let checks = store.verify(&m).unwrap();
        assert!(checks.iter().all(|c| c.shaOk == Some(true)));
        assert!(checks[0].sha256.as_deref().unwrap().len() == 64);

        // 二次下载：全部 skipped（幂等，不重下）
        let mut events2 = Vec::new();
        let o2 = store.download(&m, false, &mut |e| events2.push(e)).unwrap();
        assert_eq!((o2.downloaded, o2.skipped), (0, 2));
        assert!(events2.iter().all(|e| e.status == "skipped"));

        // force：即使已在盘也重下
        let o3 = store.download(&m, true, &mut |_| {}).unwrap();
        assert_eq!(o3.downloaded, 2);
    }

    #[test]
    fn missing_source_fails_loudly_and_leaves_no_part_file() {
        let root = tmp_root("missing");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap(); // 源目录空
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let m = fake_manifest(&other, "fake-model2");
        let store = ModelStore::new(root.join("models"), format!("file://{}", src.display()));
        let mut events = Vec::new();
        let err = store
            .download(&m, false, &mut |e| events.push(e))
            .expect_err("must fail");
        assert!(matches!(err, EmbedError::Http(_)), "got {err:?}");
        assert_eq!(events.last().unwrap().status, "failed");
        assert!(events.last().unwrap().error.is_some());
        assert!(!store.file_path(&m, &m.files[0]).exists());
        assert!(!store
            .file_path(&m, &m.files[0])
            .with_extension("part")
            .exists());
    }

    #[test]
    fn sha_mismatch_is_rejected_and_part_file_removed() {
        let root = tmp_root("shamismatch");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let m = fake_manifest(&src, "fake-model3");
        // 源内容与 manifest 不符（同大小、错内容 → sha 不符；再改大小 → 大小不符）
        std::fs::write(src.join("a.onnx"), "content-of-a.onnX").unwrap();
        let store = ModelStore::new(root.join("models"), format!("file://{}", src.display()));
        let mut events = Vec::new();
        let err = store
            .download(&m, false, &mut |e| events.push(e))
            .expect_err("must fail");
        assert!(matches!(err, EmbedError::ModelCorrupt(_)), "got {err:?}");
        assert_eq!(events.last().unwrap().status, "failed");
        assert!(!store.file_path(&m, &m.files[0]).exists());
        assert!(!store
            .file_path(&m, &m.files[0])
            .with_extension("part")
            .exists());
    }

    #[test]
    fn verify_flags_tampered_file() {
        let root = tmp_root("verify");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let m = fake_manifest(&src, "fake-model4");
        let store = ModelStore::new(root.join("models"), format!("file://{}", src.display()));
        store.download(&m, false, &mut |_| {}).unwrap();
        // 篡改：大小仍对（同长度不同内容）→ is_complete 仍 true，verify 必须发现
        let p = store.file_path(&m, &m.files[0]);
        let n = std::fs::metadata(&p).unwrap().len();
        std::fs::write(&p, vec![b'x'; n as usize]).unwrap();
        assert!(store.is_complete(&m)); // 快路径看不出
        let checks = store.verify(&m).unwrap();
        assert_eq!(checks[0].shaOk, Some(false));
        assert!(checks[1].shaOk == Some(true));
    }

    #[test]
    fn sha256_file_matches_known_vector() {
        let root = tmp_root("sha");
        let p = root.join("abc.txt");
        std::fs::write(&p, b"abc").unwrap();
        let (n, sha) = sha256_file(&p).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            sha,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
