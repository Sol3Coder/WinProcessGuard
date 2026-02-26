# WinProcessGuard

vibecoding 的windows下进程守护，服务端为rust 编写的windows 服务， 客户端是cpp类，集成到cpp程序中完成进程守护（自监控），也可以自行完成GUI部分实现对任意进程的守护。  
只进行过少量的功能测试，确认了在我的环境下正常运行，在你的环境下使用前请自行测试

```cpp
    std::string servicePath = ProcessGuard::Client::GetCurrentExeDir() + "\\processguard\\process-guard-service.exe";
    SPDLOG_INFO("服务安装状态:{}", (m_client.IsServiceInstalled() ? "是" : "否"));
    SPDLOG_INFO("服务运行状态:{}", (m_client.IsServiceRunning() ? "是" : "否"));
    if (!m_client.QuickSetup(servicePath))
    {
        SPDLOG_INFO("服务设置失败: {}", m_client.GetLastError());
        return;
    }

    auto items = m_client.GetAllMonitorItems();
    SPDLOG_INFO("获取配置项个数{}", items.size());
    if (items.empty())
    {
        SPDLOG_INFO("添加自配制，守护自身");
        m_client.AddSelfMonitor("YouAppLication", 3000);
        SPDLOG_INFO("添加自配制完成");
    }

    for (const auto &item : items)
    {
        SPDLOG_INFO("监控项名称:{}, id:{}", item.name, item.id);
        SPDLOG_INFO("监控项路径:{}", item.exePath);
        SPDLOG_INFO("监控项启用状态:{}", (item.enabled ? "是" : "否"));

        if (item.name == "YouAppLication")
        {
            m_client.SetSelfMonitorId(item.id);
            if (!item.enabled)
            {
                m_client.ResumeMonitorItem(item.id);
            }
        }
    }
SPDLOG_INFO("启动心跳，启动结果:{}", m_client.StartSelfHeartbeat(500));
```*通信**：Windows 命名管道（Named Pipe）+ JSON 协议

---

## 服务端架构

### 核心组件

#### 1. Guardian（守护核心）

`Guardian` 是服务端的核心组件，负责：

- **配置加载**：从 `config.json` 加载监控项配置
- **进程监控**：定期检查被监控进程的状态
- **心跳检测**：接收客户端心跳，检测进程是否存活
- **自动重启**：进程崩溃或心跳超时时自动重启
- **变更处理**：处理来自 PipeServer 的动态配置变更

**监控循环工作流程**：

```
启动时:
  1. 加载配置
  2. 强制启用所有监控项（enabled = true）
  3. 启动所有监控进程
  4. 进入监控循环

监控循环（每 3 秒）:
  1. 检查每个监控项:
     - 进程是否存活（通过 PID 检查）
     - 心跳是否超时（通过 last_heartbeat 检查）
  2. 如果进程异常:
     - 杀死残留进程
     - 自动重启进程
     - 记录重启次数
  3. 处理待处理的配置变更（暂停/恢复/添加/删除）
```

#### 2. PipeServer（命名管道服务）

`PipeServer` 提供 IPC 通信能力，监听命名管道 `\\.\pipe\ProcessGuardService`：

**支持的命令**：

| 命令 | 功能 | 参数 |
|------|------|------|
| `heartbeat` | 更新心跳 | `item_id` |
| `add` | 添加监控项 | `config`（完整配置） |
| `update` | 更新监控项 | `config`（完整配置） |
| `remove` | 删除监控项 | `id` |
| `stop` | 暂停监控 | `id` |
| `start` | 恢复监控 | `id` |
| `list` | 列出所有监控项 | - |
| `status` | 获取服务状态 | - |

#### 3. Session0 处理

Windows 服务运行在 Session 0（隔离会话），无法直接启动 GUI 程序。`session0.rs` 模块通过以下步骤解决：

1. 获取当前活动会话 ID（`WTSGetActiveConsoleSessionId`）
2. 获取用户令牌（`WTSQueryUserToken`）
3. 复制令牌（`DuplicateTokenEx`）
4. 创建用户环境块（`CreateEnvironmentBlock`）
5. 使用 `CreateProcessAsUserW` 在用户会话启动进程

### 服务端命令行

```bash
# 安装服务
process-guard-service.exe --install

# 卸载服务
process-guard-service.exe --uninstall

# 启动服务
process-guard-service.exe --start

# 停止服务
process-guard-service.exe --stop

# 查看状态
process-guard-service.exe --status

# 直接运行（调试用，需要管理员权限）
process-guard-service.exe
```

---

## 客户端 API

### 头文件包含

```cpp
#include "ProcessGuardClient.hpp"
```

### 命名空间

```cpp
using namespace ProcessGuard;
```

### 核心类：Client

#### 构造与析构

```cpp
// 构造函数
Client();

// 析构函数（自动断开连接、停止心跳线程）
~Client();
```

#### 静态工具方法

```cpp
// 获取当前可执行文件完整路径
static std::string GetCurrentExePath();

// 获取当前可执行文件所在目录
static std::string GetCurrentExeDir();
```

#### 服务管理

```cpp
// 检查服务是否已安装
bool IsServiceInstalled();

// 检查服务是否正在运行
bool IsServiceRunning();

// 安装服务
// @param servicePath: 服务可执行文件的完整路径
bool InstallService(const std::string &servicePath);

// 卸载服务
bool UninstallService();

// 启动服务
bool StartService();

// 停止服务
bool StopService();

// 快速设置：安装并启动服务（如果不存在/未运行）
bool QuickSetup(const std::string &servicePath);

// 确保服务已安装
bool EnsureServiceInstalled(const std::string &servicePath);

// 确保服务正在运行
bool EnsureServiceRunning();
```

#### 连接管理

```cpp
// 连接到服务端
// @param timeoutMs: 连接超时时间（毫秒），默认 5000
bool Connect(int timeoutMs = 5000);

// 断开连接
void Disconnect();

// 检查是否已连接
bool IsConnected() const;
```

#### 监控项管理

```cpp
// 添加监控项
bool AddMonitorItem(const MonitorItem &item);

// 更新监控项
bool UpdateMonitorItem(const MonitorItem &item);

// 删除监控项
bool RemoveMonitorItem(const std::string &id);

// 暂停监控项
bool StopMonitorItem(const std::string &id);
bool PauseMonitorItem(const std::string &id);  // 等同于 StopMonitorItem

// 恢复监控项
bool StartMonitorItem(const std::string &id);
bool ResumeMonitorItem(const std::string &id);  // 等同于 StartMonitorItem

// 获取所有监控项
std::vector<MonitorItem> GetAllMonitorItems();

// 获取服务状态（包含所有监控项的实时状态）
ServiceStatus GetServiceStatus();
```

#### 心跳管理

```cpp
// 发送单次心跳
bool SendHeartbeat(const std::string &itemId);

// 启动心跳线程（定期自动发送心跳）
void StartHeartbeatThread(const std::string &itemId, int intervalMs = 500);

// 停止指定监控项的心跳线程
void StopHeartbeatThread(const std::string &itemId);

// 停止所有心跳线程
void StopAllHeartbeatThreads();
```

#### 自监控专用方法

```cpp
// 添加自监控（监控当前进程）
// @param id: 监控项 ID，为空则自动生成
// @param heartbeatTimeoutMs: 心跳超时时间（毫秒），默认 24 小时
bool AddSelfMonitor(const std::string &id = "", int heartbeatTimeoutMs = 86400000);

// 删除自监控
bool RemoveSelfMonitor();

// 暂停自监控
bool PauseSelfMonitor();

// 恢复自监控
bool ResumeSelfMonitor();

// 设置自监控 ID
void SetSelfMonitorId(const std::string &id);

// 获取自监控 ID
std::string GetSelfMonitorId() const;

// 启动自心跳（便捷方法）
void StartSelfHeartbeat(int intervalMs = 500);

// 停止自心跳
void StopSelfHeartbeat();
```

#### 回调设置

```cpp
// 设置心跳失败回调
void SetHeartbeatFailedCallback(std::function<void(const std::string &)> callback);

// 设置连接状态变化回调
void SetConnectedChangedCallback(std::function<void(bool)> callback);
```

#### 错误处理

```cpp
// 获取最后一次错误信息
std::string GetLastError() const;
```

### 数据结构

#### MonitorItem（监控项配置）

```cpp
struct MonitorItem {
    std::string id;              // 唯一标识符（UUID）
    std::string exePath;         // 可执行文件路径
    std::string args;            // 启动参数（可选）
    std::string name;            // 监控项名称
    bool minimize = false;       // 是否最小化启动
    bool noWindow = false;       // 是否无窗口启动
    bool enabled = true;         // 是否启用
    int heartbeatTimeoutMs = 1000;  // 心跳超时时间（毫秒）
};
```

#### ProcessStatus（进程状态）

```cpp
struct ProcessStatus {
    std::string id;              // 监控项 ID
    std::string name;            // 监控项名称
    std::string exePath;         // 可执行文件路径
    bool enabled = false;        // 是否启用
    int processId = 0;           // 当前进程 PID
    int64_t lastHeartbeatMs = 0; // 上次心跳时间（毫秒前）
    int heartbeatTimeoutMs = 1000;
    int restartCount = 0;        // 重启次数
    bool isAlive = false;        // 进程是否存活
    bool isHeartbeatOk = false;  // 心跳是否正常
};
```

#### ServiceStatus（服务状态）

```cpp
struct ServiceStatus {
    bool serviceRunning = false;
    int totalItems = 0;
    std::vector<ProcessStatus> items;
};
```

---

## 配置文件

### 文件位置

配置文件 `config.json` 位于服务端可执行文件所在目录，首次启动时自动创建。

```
process-guard-service/
├── process-guard-service.exe
└── config.json          <-- 配置文件
```

### 文件格式

```json
{
  "items": [
    {
      "id": "550e8400-e29b-41d4-a716-446655440000",
      "exe_path": "C:\\Path\\To\\YourApp.exe",
      "args": "--arg1 --arg2",
      "name": "YourApp",
      "minimize": false,
      "no_window": false,
      "enabled": true,
      "heartbeat_timeout_ms": 3000
    }
  ]
}
```

### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `id` | string | 是 | 监控项唯一标识符，UUID 格式 |
| `exe_path` | string | 是 | 被监控程序的可执行文件完整路径 |
| `args` | string | 否 | 启动参数 |
| `name` | string | 是 | 监控项名称，用于日志显示 |
| `minimize` | boolean | 否 | 是否最小化窗口启动，默认 false |
| `no_window` | boolean | 否 | 是否无窗口启动（CREATE_NO_WINDOW），默认 false |
| `enabled` | boolean | 否 | 是否启用监控，默认 true |
| `heartbeat_timeout_ms` | number | 否 | 心跳超时时间（毫秒），默认 1000 |

### 注意事项

- **服务端启动时**：所有 `enabled=false` 的监控项会被强制设为 `enabled=true`
- **运行时动态修改**：通过客户端 API 暂停/恢复会实时修改 `enabled` 状态并保存
- **路径格式**：Windows 路径需要使用双反斜杠（`\\`）或正斜杠（`/`）

---

## 快速开始

### 1. 编译服务端

**环境要求**：Rust 1.70+

```bash
cd process-guard-service
cargo build --release
```

编译完成后，可执行文件位于 `target/release/process-guard-service.exe`

### 2. 集成客户端

将以下文件复制到你的 C++ 项目中：

- `ProcessGuardClient.hpp`
- `ProcessGuardClient.cpp`
- `nlohmann/json.hpp`（如果项目中没有）

**依赖**：
- Windows SDK
- C++17 或更高版本

### 3. 安装并启动服务

```bash
# 以管理员身份运行
process-guard-service.exe --install
process-guard-service.exe --start
```

或使用客户端代码自动安装：

```cpp
ProcessGuard::Client client;
std::string servicePath = ProcessGuard::Client::GetCurrentExeDir() + "\\process-guard-service.exe";
client.QuickSetup(servicePath);
```

### 4. 实现进程自监控

在你的应用程序初始化代码中添加：

```cpp
#include "ProcessGuardClient.hpp"
#include <spdlog/spdlog.h>

class MyApp {
    ProcessGuard::Client m_client;
    
public:
    void InitProcessGuard() {
        // 1. 设置服务路径并快速安装/启动
        std::string servicePath = ProcessGuard::Client::GetCurrentExeDir() + 
                                  "\\processguard\\process-guard-service.exe";
        
        SPDLOG_INFO("服务安装状态:{}", (m_client.IsServiceInstalled() ? "是" : "否"));
        SPDLOG_INFO("服务运行状态:{}", (m_client.IsServiceRunning() ? "是" : "否"));
        
        if (!m_client.QuickSetup(servicePath)) {
            SPDLOG_ERROR("服务设置失败: {}", m_client.GetLastError());
            return;
        }
        
        // 2. 检查是否已有自监控配置
        auto items = m_client.GetAllMonitorItems();
        SPDLOG_INFO("获取配置项个数{}", items.size());
        
        if (items.empty()) {
            // 添加自监控，守护自身
            SPDLOG_INFO("添加自监控配置");
            m_client.AddSelfMonitor("MyApplication", 3000);
            SPDLOG_INFO("添加自监控完成");
        }
        
        // 3. 查找并启用自监控
        for (const auto &item : items) {
            SPDLOG_INFO("监控项名称:{}, id:{}", item.name, item.id);
            SPDLOG_INFO("监控项路径:{}", item.exePath);
            SPDLOG_INFO("监控项启用状态:{}", (item.enabled ? "是" : "否"));
            
            if (item.name == "MyApplication") {
                m_client.SetSelfMonitorId(item.id);
                if (!item.enabled) {
                    m_client.ResumeMonitorItem(item.id);
                }
            }
        }
        
        // 4. 启动心跳（每 500ms 发送一次）
        bool heartbeatStarted = m_client.StartSelfHeartbeat(500);
        SPDLOG_INFO("启动心跳，启动结果:{}", heartbeatStarted);
    }
};
```

---

## 使用示例

### 完整自监控示例

```cpp
#include "ProcessGuardClient.hpp"
#include <spdlog/spdlog.h>
#include <iostream>

class Application {
    ProcessGuard::Client m_client;
    std::string m_selfMonitorId;
    
public:
    bool Initialize() {
        // 获取服务路径（假设服务位于子目录）
        std::string servicePath = ProcessGuard::Client::GetCurrentExeDir() + 
                                  "\\processguard\\process-guard-service.exe";
        
        // 快速设置服务
        if (!m_client.QuickSetup(servicePath)) {
            std::cerr << "服务设置失败: " << m_client.GetLastError() << std::endl;
            return false;
        }
        
        // 设置回调
        m_client.SetHeartbeatFailedCallback([this](const std::string &id) {
            SPDLOG_WARN("心跳失败，监控项: {}", id);
        });
        
        m_client.SetConnectedChangedCallback([](bool connected) {
            SPDLOG_INFO("连接状态变化: {}", connected ? "已连接" : "已断开");
        });
        
        // 获取或创建自监控
        auto items = m_client.GetAllMonitorItems();
        bool selfMonitorExists = false;
        
        for (const auto &item : items) {
            if (item.name == "MyApp") {
                m_selfMonitorId = item.id;
                m_client.SetSelfMonitorId(m_selfMonitorId);
                selfMonitorExists = true;
                
                // 如果已暂停，则恢复
                if (!item.enabled) {
                    m_client.ResumeMonitorItem(m_selfMonitorId);
                }
                break;
            }
        }
        
        if (!selfMonitorExists) {
            // 添加自监控，心跳超时 5 秒
            if (m_client.AddSelfMonitor("MyApp", 5000)) {
                m_selfMonitorId = m_client.GetSelfMonitorId();
            }
        }
        
        // 启动心跳线程
        m_client.StartSelfHeartbeat(500);
        
        return true;
    }
    
    void Shutdown() {
        // 停止心跳
        m_client.StopSelfHeartbeat();
        
        // 可选：暂停监控（不删除配置）
        // m_client.PauseSelfMonitor();
        
        // 可选：删除监控配置
        // m_client.RemoveSelfMonitor();
        
        m_client.Disconnect();
    }
    
    void PauseMonitoring() {
        m_client.PauseSelfMonitor();
        m_client.StopSelfHeartbeat();
    }
    
    void ResumeMonitoring() {
        m_client.ResumeSelfMonitor();
        m_client.StartSelfHeartbeat(500);
    }
};

int main() {
    Application app;
    
    if (!app.Initialize()) {
        return 1;
    }
    
    // 主程序逻辑...
    std::cout << "应用程序运行中，受进程守护保护..." << std::endl;
    
    // 模拟工作
    std::this_thread::sleep_for(std::chrono::seconds(60));
    
    app.Shutdown();
    return 0;
}
```

### 管理其他进程示例

```cpp
#include "ProcessGuardClient.hpp"

void ManageOtherProcess() {
    ProcessGuard::Client client;
    
    // 连接到服务
    if (!client.Connect()) {
        std::cerr << "连接失败: " << client.GetLastError() << std::endl;
        return;
    }
    
    // 添加监控项
    ProcessGuard::MonitorItem item;
    item.id = "my-worker-001";  // 指定 ID，或留空自动生成
    item.exePath = "C:\\Tools\\Worker.exe";
    item.name = "WorkerProcess";
    item.args = "--mode=production --port=8080";
    item.minimize = true;       // 最小化启动
    item.heartbeatTimeoutMs = 5000;  // 5 秒心跳超时
    
    if (client.AddMonitorItem(item)) {
        std::cout << "监控项添加成功" << std::endl;
    }
    
    // 获取服务状态
    auto status = client.GetServiceStatus();
    std::cout << "服务运行中: " << status.serviceRunning << std::endl;
    std::cout << "监控项数量: " << status.totalItems << std::endl;
    
    for (const auto &ps : status.items) {
        std::cout << "进程: " << ps.name 
                  << ", PID: " << ps.processId
                  << ", 存活: " << ps.isAlive
                  << ", 心跳正常: " << ps.isHeartbeatOk
                  << ", 重启次数: " << ps.restartCount << std::endl;
    }
    
    // 暂停监控
    client.PauseMonitorItem("my-worker-001");
    
    // 恢复监控
    client.ResumeMonitorItem("my-worker-001");
    
    // 删除监控
    // client.RemoveMonitorItem("my-worker-001");
}
```

---

## 常见问题

### Q: 服务安装失败？

确保以**管理员身份**运行程序，或检查是否有杀毒软件拦截。

### Q: 进程无法启动（GUI 程序不显示窗口）？

服务端会自动处理 Session 0 隔离，确保 GUI 程序在用户会话中启动。如果仍有问题，检查：
- 可执行文件路径是否正确
- 用户是否有权限访问该路径
- 程序是否需要特定的工作目录

### Q: 心跳超时导致进程被重启？

- 检查心跳间隔是否小于超时时间：`intervalMs < heartbeatTimeoutMs`
- 确保程序正常退出前调用 `StopSelfHeartbeat()`
- 对于正常关闭的场景，可以先调用 `PauseSelfMonitor()` 暂停监控

### Q: 配置文件在哪里？

`config.json` 位于 `process-guard-service.exe` 所在目录，首次启动时自动创建。

### Q: 如何查看服务日志？

服务日志位于 `process-guard-service.exe` 所在目录的 `process-guard-service.log` 文件中。

---

## 许可证

MIT License
