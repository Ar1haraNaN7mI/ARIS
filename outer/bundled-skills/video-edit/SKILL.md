---
name: video-edit
description: 剪辑和拼接视频，选择素材片段、保留或静音音轨，导出并检查 MP4。Cut and concatenate local video clips.
tags: [剪辑, 拼接, 视频, video, edit, media]
---

从工作区读取用户提供的素材，使用 `media_status` 检查 FFmpeg 和 ffprobe，再用 `media_probe` 查实际时长、分辨率和音轨。把剪辑要求落实到片段起点、持续时间和播放顺序。每个区间都应落在源文件的实际时长内。

`video_edit` 接收 `clips` 和新的 MP4 输出路径。每个片段提供 `path`、`start_seconds` 和 `duration_seconds`。可设置输出宽高、帧率和 `audio`。保留音轨时使用 `preserve`，静音时使用 `mute`。画幅不同的片段以留边适配输出尺寸。

```json
{"clips":[{"path":"input/interview.mp4","start_seconds":12,"duration_seconds":8},{"path":"input/demo.mp4","start_seconds":0,"duration_seconds":15}],"output":"deliverables/edit-v1.mp4","width":1920,"height":1080,"fps":30,"audio":"preserve"}
```

工具支持最多 12 个片段和 600 秒的总时长。需要字幕、转场或更复杂的时间线时，先检查已安装的 FFmpeg 滤镜及可用的工作区脚本工具。用新文件名导出，交付实际路径、时长、分辨率和音轨状态。
