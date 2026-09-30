# ffmpeg, for the video step / 给视频这一步用的 ffmpeg

**What this is.** The recorder captures screenshots; the video you watch is encoded from them by ffmpeg.
The application carries no ffmpeg of its own, so on a machine that has never had one, screenshots pile up in
`cache_screenshot\` and no `.mp4` is ever finished. This archive is that missing piece, as one file — plus
the licence that came with it.

**这是什么。** 录制器抓的是截图，你看到的视频是用这些截图编码出来的。应用本身不带 ffmpeg，所以在一台
从来没装过 ffmpeg 的机器上，截图会一直堆在 `cache_screenshot\` 里，永远等不到一个 `.mp4`。
这个压缩包就是缺的那一块——一个文件，外加跟它一起来的许可证。

**Where it goes / 放在哪里.** Unzip this archive **into the app folder** — the one holding `bin\` — which
puts `ffmpeg.exe` where the application looks first. Nothing else is touched and no `PATH` is modified.

解压到**应用目录**（就是放着 `bin\` 的那一层），`ffmpeg.exe` 就落在应用第一个去找的位置。
不碰其它文件，也不改 `PATH`。

**Prove it / 验证它.**

```powershell
.\ffmpeg.exe -version
bin\windsetup.exe doctor --root .
```

The first line is the binary answering for itself. The second is the application's own report: the
`VIDEO STEP` line says which ffmpeg it resolved and **where it came from** — `this install` when it found
the copy you just unpacked, `from PATH` when the app folder has none and it fell back to a system-wide one,
and a plain `no ffmpeg` with the consequence spelled out when there is nothing to find.

第一行是这个二进制自己回答。第二行是应用自己的报告：`VIDEO STEP` 那一行会说明它解析到了哪个 ffmpeg、
**从哪里来的**——刚解压的这一份在应用目录里，它就写 `this install`；应用目录里没有、退回到系统装的，它写
`from PATH`；什么都没找到，它就直说 `no ffmpeg`，并且把后果讲清楚。

**Provenance / 来源.** A stock Windows x86-64 build from <https://www.gyan.dev/ffmpeg/>, unmodified —
nothing in this archive was compiled or patched by this project. The build's version is in the archive's
name, every byte's hash is in `MANIFEST.sha256` beside this file, and `LICENSE` is the licence that build
ships with (GPL version 3, which is why this is a separate download and not part of the GPL-2.0
application zip: two files, two licences, each with its own terms).

**来源。** 来自 <https://www.gyan.dev/ffmpeg/> 的标准 Windows x86-64 构建，未经任何修改——这里的文件
没有一个是被本项目编译或打过补丁的。构建版本号在压缩包名字里，每个字节的哈希在同目录的
`MANIFEST.sha256` 里，`LICENSE` 就是这个构建自带的许可证文本（GPL version 3；这也是它单独作为一个下载、
而不是塞进 GPL-2.0 应用包的原因：两份文件、两份许可证，各自独立）。
