# 在 ARIS 里剪视频、做海报和生成图片

把素材放进工作区，像发一条任务消息那样描述交付物。ARIS 使用本地 FFmpeg 剪视频，使用 Pillow 排海报，通过配置的 KEI 模型生成图片。写文件和调用计费接口遵循当前工作区的工具权限。

| 在对话中输入 | 交付 |
| --- | --- |
| 把 input/a.mp4 的第 1 到第 4 秒和 input/b.mp4 的前 2 秒拼成一个 1280×720 的视频，保留音频 | 一个新的 H.264 MP4，附导出时长和音轨信息 |
| 为周五的开放日做一张 1080×1350 中文海报，使用工作区里的活动照片 | 排版后的 PNG，附实际使用的字体 |
| 用 KEI 生成一张天青色玻璃棱镜主视觉，保存为 deliverables/prism.png | 从网关返回并在本地解码、验证过的 PNG |

## 安装与素材

标准安装脚本使用 Python 3.11 或更新版本，把固定版本的 Pillow 安装在 `share/aris/media/.venv`。剪辑另外需要 PATH 能找到 `ffmpeg` 和 `ffprobe`，也可分别通过 `ARIS_FFMPEG` 和 `ARIS_FFPROBE` 指定。开发时可用 `ARIS_MEDIA_PYTHON` 选择解释器，用 `ARIS_MEDIA_RUNTIME` 选择适配器。运行 `media_status` 可检查当前环境。[FFmpeg 下载](https://ffmpeg.org/download.html)

视频可以组合最多 12 个片段，总时长上限 600 秒，支持 1 到 60 fps，宽高为偶数，最大 3840×2160。各片段按比例缩放，以留边适配画布。保留音频时统一编码为 AAC，也可选择静音。海报最大 4096×4096，支持 80 个文字、矩形和图片元素。中文默认选用机器上可用的微软雅黑、苹方或 Noto CJK。指定品牌字体时，把字体文件放在工作区并提供相对路径。

## 接通 KEI 图像生成

在 ARIS 的用户配置目录（通常是 `~/.aris`）创建 `media.json`，修改后重启工作区。端点取自 KEI 设置，`api_key_env` 填写已导入的凭据引用或环境变量名。

以下是通过 KEI 的 Responses 主模型调用公开图像工具的配置。将凭据引用替换为本机已获授权的引用。运营方如需固定底层图像模型，可另设 `image_model`。

```json
{
  "image": {
    "enabled": true,
    "protocol": "responses",
    "model": "gpt-6.1-sol",
    "api_key_env": "KEI_API_KEY"
  }
}
```

这一模式通过 `/responses` 指定 `image_generation` 工具。ARIS 读取一条已完成的 `image_generation_call.result`，校验图像内容后写入新文件。验收使用上述主模型在 KEI 完成了真实调用，其他账号能用的模型取决于相应 Key 的通道。[Responses 图像生成](https://developers.openai.com/api/docs/guides/tools-image-generation)

使用独立图像模型时，将 `protocol` 设为 `images`，`model` 填写该 Key 可以调用的精确图像模型 ID。ARIS 向 `/images/generations` 请求一张图片，读取 `data[0].b64_json`。仅在供应商明确要求时添加 `"response_format": "b64_json"`。两种协议都在工作区发布 PNG，可以向模型指定供应商支持的尺寸和质量。每次交付使用新文件名。[Images API](https://developers.openai.com/api/reference/resources/images/methods/generate)

## 验证

`python -m unittest discover -s scripts -p test_media_tools.py` 验证实际视频剪切、海报渲染、两个图像协议和文件路径。运行 `cargo test -p aris-core media::tests` 可验证 Rust 工具的参数及文件访问约束。
