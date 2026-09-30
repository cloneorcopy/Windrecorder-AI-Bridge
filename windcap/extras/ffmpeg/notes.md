### ffmpeg {BUILD}，给视频这一步 / for the video step

{TOTAL}（打包后 {ZIP}，sha256 `{SHA256}`）。**解压到应用目录**——就是放着 `bin\` 的那一层——
`ffmpeg.exe` 就落在应用第一个去找的位置。运行 `bin\windsetup.exe doctor --root .`，
`VIDEO STEP` 那一行会写出它解析到的 ffmpeg 以及**来自哪里**（`this install` 就是刚解压的这一份）。
录制器抓的是截图，视频是编码出来的；应用本身不带 ffmpeg，所以一台从没装过的机器上，截图会一直堆在
`cache_screenshot\` 里，永远等不到 `.mp4`——这一步不会报错，只会安静地不出视频。
已经装了 ffmpeg 的机器不需要这个下载：应用会退回到 `PATH` 上那一个，`doctor` 会写 `from PATH`。
里面是 <{SOURCE}> 未经修改的 Windows x86-64 构建，版本号在文件名里，每个字节的哈希在压缩包内的
`MANIFEST.sha256` 里；`LICENSE` 是这个构建自带的 GPL version 3 文本——所以它是单独一个下载，
不塞进 GPL-2.0 的应用包里：两份文件、两份许可证。

ffmpeg {BUILD} — {TOTAL} ({ZIP} compressed, sha256 `{SHA256}`). **Unzip into the app folder**, the one
holding `bin\`, which puts `ffmpeg.exe` where the application looks first. Run
`bin\windsetup.exe doctor --root .` and the `VIDEO STEP` line names the ffmpeg it resolved and **where it
came from** (`this install` being the copy you just unpacked). The recorder captures screenshots and the
video is encoded from them; the application carries no ffmpeg of its own, so on a machine that never had
one, screenshots pile up in `cache_screenshot\` and no `.mp4` is ever finished — and nothing raises an
error, it simply never produces a video. A machine that already has ffmpeg does not need this download: the
app falls back to the one on `PATH`, and `doctor` says `from PATH`. Inside is an unmodified Windows x86-64
build from <{SOURCE}>, its version in the file name and every byte's hash in the archive's
`MANIFEST.sha256`; `LICENSE` is the GPL version 3 text that build ships with, which is why this is its own
download rather than part of the GPL-2.0 application zip — two files, two licences.
