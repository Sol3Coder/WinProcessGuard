#pragma once

#include <string>
#include <vector>
#include <functional>
#include <thread>
#include <mutex>
#include <atomic>
#include <memory>
#include <chrono>
#include <map>

#ifdef _WIN32
#include <Windows.h>
#endif

namespace ProcessGuard
{

    struct MonitorItem
    {
        std::string id;
        std::string exePath;
        std::string args;
        std::string name;
        bool minimize = false;
        bool noWindow = false;
        bool enabled = true;
        int heartbeatTimeoutMs = 1000;

        MonitorItem() = default;

        MonitorItem(const std::string &id, const std::string &exePath, const std::string &name)
            : id(id), exePath(exePath), name(name) {}

        static MonitorItem Create(const std::string &exePath, const std::string &name,
                                  const std::string &id = "")
        {
            MonitorItem item;
            item.id = id.empty() ? GenerateId() : id;
            item.exePath = exePath;
            item.name = name;
            return item;
        }

    private:
        static std::string GenerateId()
        {
            auto now = std::chrono::system_clock::now();
            auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                          now.time_since_epoch())
                          .count();
            return "item-" + std::to_string(ms);
        }
    };

    struct ProcessStatus
    {
        std::string id;
        std::string name;
        std::string exePath;
        bool enabled = false;
        int processId = 0;
        int64_t lastHeartbeatMs = 0;
        int heartbeatTimeoutMs = 1000;
        int restartCount = 0;
        bool isAlive = false;
        bool isHeartbeatOk = false;
    };

    struct ServiceStatus
    {
        bool serviceRunning = false;
        int totalItems = 0;
        std::vector<ProcessStatus> items;
    };

    class Client
    {
    public:
        Client();
        ~Client();

        Client(const Client &) = delete;
        Client &operator=(const Client &) = delete;

        static std::string GetCurrentExePath();
        static std::string GetCurrentExeDir();

        std::string GetLastError() const;

        bool IsServiceInstalled();
        bool IsServiceRunning();
        bool InstallService(const std::string &servicePath);
        bool UninstallService();
        bool StartService();
        bool StopService();

        bool QuickSetup(const std::string &servicePath);

        bool Connect(int timeoutMs = 5000);
        void Disconnect();
        bool IsConnected() const;

        bool AddMonitorItem(const MonitorItem &item);
        bool UpdateMonitorItem(const MonitorItem &item);
        bool RemoveMonitorItem(const std::string &id);
        bool StopMonitorItem(const std::string &id);
        bool StartMonitorItem(const std::string &id);
        bool PauseMonitorItem(const std::string &id) { return StopMonitorItem(id); }
        bool ResumeMonitorItem(const std::string &id) { return StartMonitorItem(id); }

        std::vector<MonitorItem> GetAllMonitorItems();
        ServiceStatus GetServiceStatus();

        bool SendHeartbeat(const std::string &itemId);
        void StartHeartbeatThread(const std::string &itemId, int intervalMs = 500);
        void StopHeartbeatThread(const std::string &itemId);
        void StopAllHeartbeatThreads();

        bool EnsureServiceInstalled(const std::string &servicePath);
        bool EnsureServiceRunning();

        void SetHeartbeatFailedCallback(std::function<void(const std::string &)> callback);
        void SetConnectedChangedCallback(std::function<void(bool)> callback);

        bool AddSelfMonitor(const std::string &id = "", int heartbeatTimeoutMs = 86400000);
        bool RemoveSelfMonitor();
        bool PauseSelfMonitor();
        bool ResumeSelfMonitor();
        void SetSelfMonitorId(const std::string &id);
        std::string GetSelfMonitorId() const;
        void StartSelfHeartbeat(int intervalMs = 500);
        void StopSelfHeartbeat();

    private:
        struct Impl;
        std::unique_ptr<Impl> impl_;
    };

}
