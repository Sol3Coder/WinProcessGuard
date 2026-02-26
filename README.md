# WinProcessGuard

vibecoding 的windows下进程守护，服务端为rust 编写的windows 服务，  客户端是cpp类，集成到cpp程序中完成进程守护（自监控），也可以自行完成GUI部分实现对任意进程的守护。  

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
```
