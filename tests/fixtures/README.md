# 测试 fixture 图片（~40KB × 2，随仓库分发，无需 LFS）

真模型语义测试（`crates/engine-embed/tests/real_siglip.rs`）需要真实照片，不使用合成图：
合成纯色块对图文语义无意义（SigLIP 对 "a dog" / "a car" 的排序必须建立在真实内容上）。

| 文件 | 内容 | 来源 | 许可 |
|------|------|------|------|
| `dog.jpg` | 草地上的金毛寻回犬（330×231） | Wikimedia Commons `File:Lily the Golden Retriever in the grass.jpg` | Public domain |
| `car.jpg` | 红色 Smart 汽车侧视图（330×247） | Wikimedia Commons `File:Red Smart Car Side View Driveway.jpg` | CC0 |

出处页面：
- https://commons.wikimedia.org/wiki/File:Lily_the_Golden_Retriever_in_the_grass.jpg
- https://commons.wikimedia.org/wiki/File:Red_Smart_Car_Side_View_Driveway.jpg

图片来源由 Wikimedia Commons API 检索取得（`scripts/fetch_test_fixtures.sh` 记录了完整取图命令，
可重复下载、可换图）。两个文件均为 330px 缩略图，许可允许随源码分发（PD / CC0 无署名义务，
此处仍标注出处）。

## 约束

- 图片必须真实且语义明确（狗 / 汽车），否则语义断言（`cos(狗图,"一只狗") > cos(狗图,"一辆汽车")`）
  失去意义。
- 保持小尺寸（≤ 400px / ≤ 100KB）：fixture 进仓库，`git` 里不该出现大文件。
- 换图 = 换语义断言的基准，必须同时跑 `cargo test -p engine-embed --test real_siglip` 确认全绿。
