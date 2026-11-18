---
name: visual-design
description: 画海报、封面、插画或生成图片。Precisely lay out a poster, generate original artwork, or combine both into a finished PNG.
tags: [海报, 插画, 生图, 封面, poster, image, visual]
---

先明确交付尺寸、用途和必须逐字保留的文案。用 `media_status` 检查本地渲染器与 KEI 模型配置。文字位置和品牌版式交给 `poster_render`。需要原创照片、插画或背景时，用 `image_generate` 取得原图，再按需叠加标题和标识。

`image_generate` 接收 `prompt`、新 PNG 的 `output` 路径，以及可选的 `size` 和 `quality`。提示词描述主体、构图、光线和色彩。端点、模型和凭据取自运行时配置。生成失败时说明影响交付的原因，保留已经完成的素材。

`poster_render` 需要画布宽高、输出路径和从底层到顶层排列的 `elements`。所有元素使用像素坐标。文字可设置字体大小、颜色和显式换行，矩形与图片需要宽高。本地图片会裁切填满对应矩形。长标题先划分行宽，再检查留白和对比度。中文可以使用系统内置字体，品牌字体用工作区内的 TTF、OTF 或 TTC 文件。渲染溢出时调整版式后重新导出。

用 `media_probe` 核对成品尺寸，交付工具返回的路径和实际尺寸。若生成底图和正式海报各有用途，清楚标出两份文件。
