亦安快速启动 - 捆绑 Everything 运行时（放在与 yian-launcher.exe 同级目录）

必需文件：
  Everything.exe     - Everything 主程序（便携托盘运行）
  Everything64.dll   - Everything SDK（x64）
  es.exe             - 命令行搜索备用
  Everything.ini     - 便携配置
  Everything.lng     - 语言包（可选）

编译时 build.rs 会自动复制到 target/release/（或 debug/）。
部署时请将这些文件与 yian-launcher.exe 放在同一文件夹。
