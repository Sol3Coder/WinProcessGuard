#include "ProcessGuardClient.hpp"
#include <sstream>
#include <filesystem>
#include <condition_variable>

#ifdef _WIN32
#include <winsvc.h>
#endif

#ifndef NLOHMANN_JSON_HPP_INCLUDED
#include "nlohmann/json.hpp"
#endif

namespace ProcessGuard
{

    static const char *SERVICE_NAME = "ProcessGuardService";
    static const char *PIPE_NAME = "\\\\.\\pipe\\ProcessGuardService";
    static const size_t PIPE_BUFFER_SIZE = 65536;

    class PipeClient
    {
    public:
        PipeClient() : pipeHandle_(INVALID_HANDLE_VALUE), connected_(false) {}

        ~PipeClient() { Disconnect(); }

        bool Connect(int timeoutMs)
        {
            std::lock_guard<std::mutex> lock(mutex_);
            DisconnectInternal();

            DWORD startTime = GetTickCount();

            while (true)
            {
                HANDLE pipe = CreateFileA(
                    PIPE_NAME,
                    GENERIC_READ | GENERIC_WRITE,
                    0, nullptr, OPEN_EXISTING, 0, nullptr);

                if (pipe != INVALID_HANDLE_VALUE)
                {
                    DWORD mode = PIPE_READMODE_BYTE;
                    if (SetNamedPipeHandleState(pipe, &mode, nullptr, nullptr))
                    {
                        pipeHandle_ = pipe;
                        connected_ = true;
                        return true;
                    }
                    CloseHandle(pipe);
                }

                DWORD error = GetLastError();
                if (error != ERROR_PIPE_BUSY)
                {
                    DWORD elapsed = GetTickCount() - startTime;
                    if (elapsed >= static_cast<DWORD>(timeoutMs))
                        break;

                    WaitNamedPipeA(PIPE_NAME, timeoutMs - elapsed);
                    elapsed = GetTickCount() - startTime;
                    if (elapsed >= static_cast<DWORD>(timeoutMs))
                        break;
                }
            }

            return false;
        }

        void Disconnect()
        {
            std::lock_guard<std::mutex> lock(mutex_);
            DisconnectInternal();
        }

        bool IsConnected() const { return connected_; }

        nlohmann::json SendRequest(const nlohmann::json &request)
        {
            std::lock_guard<std::mutex> lock(mutex_);

            if (!connected_)
            {
                return {{"success", false}, {"message", "Not connected"}};
            }

            std::string requestStr = request.dump();
            DWORD bytesWritten = 0;

            if (!WriteFile(pipeHandle_, requestStr.c_str(),
                           static_cast<DWORD>(requestStr.length()), &bytesWritten, nullptr) ||
                bytesWritten != requestStr.length())
            {
                DisconnectInternal();
                return {{"success", false}, {"message", "Write failed"}};
            }

            char buffer[PIPE_BUFFER_SIZE] = {0};
            DWORD bytesRead = 0;

            if (!ReadFile(pipeHandle_, buffer, PIPE_BUFFER_SIZE - 1, &bytesRead, nullptr) ||
                bytesRead == 0)
            {
                DisconnectInternal();
                return {{"success", false}, {"message", "Read failed"}};
            }

            buffer[bytesRead] = '\0';

            DisconnectInternal();

            try
            {
                return nlohmann::json::parse(buffer);
            }
            catch (const std::exception &e)
            {
                return {{"success", false}, {"message", std::string("Parse error: ") + e.what()}};
            }
            catch (...)
            {
                return {{"success", false}, {"message", "Unknown parse error"}};
            }
        }

    private:
        HANDLE pipeHandle_;
        bool connected_;
        std::mutex mutex_;

        void DisconnectInternal()
        {
            if (pipeHandle_ != INVALID_HANDLE_VALUE)
            {
                CloseHandle(pipeHandle_);
                pipeHandle_ = INVALID_HANDLE_VALUE;
            }
            connected_ = false;
        }
    };

    class ServiceManager
    {
    public:
        bool IsServiceInstalled()
        {
            SC_HANDLE scm = OpenSCManagerA(nullptr, nullptr, SC_MANAGER_CONNECT);
            if (!scm)
                return false;

            SC_HANDLE svc = OpenServiceA(scm, SERVICE_NAME, SERVICE_QUERY_STATUS);
            bool result = (svc != nullptr);

            if (svc)
                CloseServiceHandle(svc);
            CloseServiceHandle(scm);
            return result;
        }

        bool IsServiceRunning()
        {
            SC_HANDLE scm = OpenSCManagerA(nullptr, nullptr, SC_MANAGER_CONNECT);
            if (!scm)
                return false;

            SC_HANDLE svc = OpenServiceA(scm, SERVICE_NAME, SERVICE_QUERY_STATUS);
            if (!svc)
            {
                CloseServiceHandle(scm);
                return false;
            }

            SERVICE_STATUS status;
            bool running = QueryServiceStatus(svc, &status) &&
                           status.dwCurrentState == SERVICE_RUNNING;

            CloseServiceHandle(svc);
            CloseServiceHandle(scm);
            return running;
        }

        bool InstallService(const std::string &servicePath, std::string &error)
        {
            SC_HANDLE scm = OpenSCManagerA(nullptr, nullptr, SC_MANAGER_CREATE_SERVICE);
            if (!scm)
            {
                error = "Failed to open SCM";
                return false;
            }

            SC_HANDLE svc = CreateServiceA(
                scm, SERVICE_NAME, "Process Guard Service",
                SERVICE_ALL_ACCESS, SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START, SERVICE_ERROR_NORMAL,
                servicePath.c_str(), nullptr, nullptr, nullptr, nullptr, nullptr);

            if (!svc)
            {
                DWORD err = GetLastError();
                error = (err == ERROR_SERVICE_EXISTS) ? "Service already exists" : "Failed to create service: " + std::to_string(err);
                CloseServiceHandle(scm);
                return false;
            }

            CloseServiceHandle(svc);
            CloseServiceHandle(scm);
            return true;
        }

        bool UninstallService(std::string &error)
        {
            SC_HANDLE scm = OpenSCManagerA(nullptr, nullptr, SC_MANAGER_CONNECT);
            if (!scm)
            {
                error = "Failed to open SCM";
                return false;
            }

            SC_HANDLE svc = OpenServiceA(scm, SERVICE_NAME,
                                         SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE);
            if (!svc)
            {
                error = "Service not found";
                CloseServiceHandle(scm);
                return false;
            }

            SERVICE_STATUS status;
            if (QueryServiceStatus(svc, &status) &&
                status.dwCurrentState == SERVICE_RUNNING)
            {
                ControlService(svc, SERVICE_CONTROL_STOP, &status);
                Sleep(1000);
            }

            bool result = DeleteService(svc);
            if (!result)
            {
                error = "Failed to delete service";
            }

            CloseServiceHandle(svc);
            CloseServiceHandle(scm);
            return result;
        }

        bool StartService_(std::string &error)
        {
            SC_HANDLE scm = OpenSCManagerA(nullptr, nullptr, SC_MANAGER_CONNECT);
            if (!scm)
            {
                error = "Failed to open SCM";
                return false;
            }

            SC_HANDLE svc = OpenServiceA(scm, SERVICE_NAME, SERVICE_START | SERVICE_QUERY_STATUS);
            if (!svc)
            {
                error = "Service not found";
                CloseServiceHandle(scm);
                return false;
            }

            BOOL result = ::StartServiceA(svc, 0, nullptr);
            if (!result && GetLastError() != ERROR_SERVICE_ALREADY_RUNNING)
            {
                error = "Failed to start service";
                CloseServiceHandle(svc);
                CloseServiceHandle(scm);
                return false;
            }

            for (int i = 0; i < 60; i++)
            {
                SERVICE_STATUS status;
                if (QueryServiceStatus(svc, &status) &&
                    status.dwCurrentState == SERVICE_RUNNING)
                {
                    break;
                }
                Sleep(500);
            }

            CloseServiceHandle(svc);
            CloseServiceHandle(scm);
            return true;
        }

        bool StopService_(std::string &error)
        {
            SC_HANDLE scm = OpenSCManagerA(nullptr, nullptr, SC_MANAGER_CONNECT);
            if (!scm)
            {
                error = "Failed to open SCM";
                return false;
            }

            SC_HANDLE svc = OpenServiceA(scm, SERVICE_NAME, SERVICE_STOP | SERVICE_QUERY_STATUS);
            if (!svc)
            {
                error = "Service not found";
                CloseServiceHandle(scm);
                return false;
            }

            SERVICE_STATUS status;
            BOOL result = ControlService(svc, SERVICE_CONTROL_STOP, &status);

            if (result)
            {
                for (int i = 0; i < 60; i++)
                {
                    if (QueryServiceStatus(svc, &status) &&
                        status.dwCurrentState == SERVICE_STOPPED)
                    {
                        break;
                    }
                    Sleep(500);
                }
            }

            CloseServiceHandle(svc);
            CloseServiceHandle(scm);
            return true;
        }
    };

    struct Client::Impl
    {
        std::unique_ptr<PipeClient> pipeClient;
        std::unique_ptr<ServiceManager> serviceManager;

        std::mutex heartbeatMutex;
        std::map<std::string, std::unique_ptr<std::thread>> heartbeatThreads;
        std::map<std::string, std::atomic<bool>> heartbeatRunning;

        std::function<void(const std::string &)> heartbeatFailedCallback;
        std::function<void(bool)> connectedChangedCallback;

        std::atomic<bool> connected{false};
        mutable std::string lastError;
        std::string selfMonitorId;

        Impl() : pipeClient(std::make_unique<PipeClient>()),
                 serviceManager(std::make_unique<ServiceManager>()) {}
    };

    Client::Client() : impl_(std::make_unique<Impl>()) {}

    Client::~Client()
    {
        StopAllHeartbeatThreads();
        Disconnect();
    }

    std::string Client::GetCurrentExePath()
    {
        char buffer[MAX_PATH] = {0};
        DWORD result = GetModuleFileNameA(nullptr, buffer, MAX_PATH);
        if (result == 0 || result >= MAX_PATH)
        {
            return "";
        }
        return std::string(buffer);
    }

    std::string Client::GetCurrentExeDir()
    {
        std::string exePath = GetCurrentExePath();
        size_t pos = exePath.find_last_of("\\/");
        return (pos != std::string::npos) ? exePath.substr(0, pos) : exePath;
    }

    std::string Client::GetLastError() const
    {
        return impl_->lastError;
    }

    bool Client::IsServiceInstalled()
    {
        return impl_->serviceManager->IsServiceInstalled();
    }

    bool Client::IsServiceRunning()
    {
        return impl_->serviceManager->IsServiceRunning();
    }

    bool Client::InstallService(const std::string &servicePath)
    {
        std::string error;
        bool result = impl_->serviceManager->InstallService(servicePath, error);
        if (!result)
            impl_->lastError = error;
        return result;
    }

    bool Client::UninstallService()
    {
        std::string error;
        bool result = impl_->serviceManager->UninstallService(error);
        if (!result)
            impl_->lastError = error;
        return result;
    }

    bool Client::StartService()
    {
        std::string error;
        bool result = impl_->serviceManager->StartService_(error);
        if (!result)
            impl_->lastError = error;
        return result;
    }

    bool Client::StopService()
    {
        std::string error;
        bool result = impl_->serviceManager->StopService_(error);
        if (!result)
            impl_->lastError = error;
        return result;
    }

    bool Client::QuickSetup(const std::string &servicePath)
    {
        if (!IsServiceInstalled() && !InstallService(servicePath))
            return false;
        if (!IsServiceRunning() && !StartService())
            return false;
        return true;
    }

    bool Client::Connect(int timeoutMs)
    {
        bool result = impl_->pipeClient->Connect(timeoutMs);
        impl_->connected = result;
        if (!result)
            impl_->lastError = "Failed to connect to service pipe";
        if (impl_->connectedChangedCallback)
            impl_->connectedChangedCallback(result);
        return result;
    }

    void Client::Disconnect()
    {
        impl_->pipeClient->Disconnect();
        impl_->connected = false;
        if (impl_->connectedChangedCallback)
            impl_->connectedChangedCallback(false);
    }

    bool Client::IsConnected() const
    {
        return impl_->connected;
    }

    bool Client::AddMonitorItem(const MonitorItem &item)
    {
        if (!impl_->connected && !Connect())
            return false;

        if (item.id.empty())
        {
            impl_->lastError = "Item ID cannot be empty";
            return false;
        }
        if (item.exePath.empty())
        {
            impl_->lastError = "Executable path cannot be empty";
            return false;
        }
        if (item.name.empty())
        {
            impl_->lastError = "Item name cannot be empty";
            return false;
        }

        try
        {
            nlohmann::json listRequest;
            listRequest["type"] = "list";
            auto listResponse = impl_->pipeClient->SendRequest(listRequest);
            impl_->connected = impl_->pipeClient->IsConnected();

            if (listResponse.is_object() && listResponse.contains("data") && listResponse["data"].is_array())
            {
                for (const auto &existingItem : listResponse["data"])
                {
                    std::string existingPath = existingItem.value("exe_path", "");
                    if (!existingPath.empty() && existingPath == item.exePath)
                    {
                        impl_->lastError = "Executable path already monitored";
                        return false;
                    }
                }
            }

            if (!impl_->connected && !Connect())
            {
                impl_->lastError = "Failed to reconnect to service";
                return false;
            }

            nlohmann::json request;
            request["type"] = "add";

            nlohmann::json config;
            config["id"] = item.id;
            config["exe_path"] = item.exePath;
            config["name"] = item.name;
            config["minimize"] = item.minimize;
            config["no_window"] = item.noWindow;
            config["enabled"] = item.enabled;
            config["heartbeat_timeout_ms"] = static_cast<int64_t>(item.heartbeatTimeoutMs);
            if (!item.args.empty())
            {
                config["args"] = item.args;
            }
            request["config"] = config;

            auto response = impl_->pipeClient->SendRequest(request);
            impl_->connected = impl_->pipeClient->IsConnected();
            if (!response.is_object() || !response.value("success", false))
            {
                impl_->lastError = response.value("message", "Unknown error");
                return false;
            }
            return true;
        }
        catch (const std::exception &e)
        {
            impl_->connected = false;
            impl_->lastError = std::string("AddMonitorItem error: ") + e.what();
            return false;
        }
        catch (...)
        {
            impl_->connected = false;
            impl_->lastError = "AddMonitorItem unknown error";
            return false;
        }
    }

    bool Client::UpdateMonitorItem(const MonitorItem &item)
    {
        if (!impl_->connected && !Connect())
            return false;

        try
        {
            nlohmann::json request;
            request["type"] = "update";

            nlohmann::json config;
            config["id"] = item.id;
            config["exe_path"] = item.exePath;
            config["name"] = item.name;
            config["minimize"] = item.minimize;
            config["no_window"] = item.noWindow;
            config["enabled"] = item.enabled;
            config["heartbeat_timeout_ms"] = item.heartbeatTimeoutMs;
            if (!item.args.empty())
            {
                config["args"] = item.args;
            }
            request["config"] = config;

            auto response = impl_->pipeClient->SendRequest(request);
            impl_->connected = impl_->pipeClient->IsConnected();
            if (!response.is_object() || !response.value("success", false))
            {
                impl_->lastError = response.value("message", "Unknown error");
                return false;
            }
            return true;
        }
        catch (const std::exception &e)
        {
            impl_->connected = false;
            impl_->lastError = std::string("UpdateMonitorItem error: ") + e.what();
            return false;
        }
        catch (...)
        {
            impl_->connected = false;
            impl_->lastError = "UpdateMonitorItem unknown error";
            return false;
        }
    }

    bool Client::RemoveMonitorItem(const std::string &id)
    {
        if (!impl_->connected && !Connect())
            return false;

        try
        {
            nlohmann::json request;
            request["type"] = "remove";
            request["id"] = id;

            auto response = impl_->pipeClient->SendRequest(request);
            impl_->connected = impl_->pipeClient->IsConnected();
            if (!response.is_object() || !response.value("success", false))
            {
                impl_->lastError = response.value("message", "Unknown error");
                return false;
            }
            return true;
        }
        catch (const std::exception &e)
        {
            impl_->connected = false;
            impl_->lastError = std::string("RemoveMonitorItem error: ") + e.what();
            return false;
        }
        catch (...)
        {
            impl_->connected = false;
            impl_->lastError = "RemoveMonitorItem unknown error";
            return false;
        }
    }

    bool Client::StopMonitorItem(const std::string &id)
    {
        if (!impl_->connected && !Connect())
            return false;

        try
        {
            nlohmann::json request;
            request["type"] = "stop";
            request["id"] = id;

            auto response = impl_->pipeClient->SendRequest(request);
            impl_->connected = impl_->pipeClient->IsConnected();
            if (!response.is_object() || !response.value("success", false))
            {
                impl_->lastError = response.value("message", "Unknown error");
                return false;
            }
            return true;
        }
        catch (const std::exception &e)
        {
            impl_->connected = false;
            impl_->lastError = std::string("StopMonitorItem error: ") + e.what();
            return false;
        }
        catch (...)
        {
            impl_->connected = false;
            impl_->lastError = "StopMonitorItem unknown error";
            return false;
        }
    }

    bool Client::StartMonitorItem(const std::string &id)
    {
        if (!impl_->connected && !Connect())
            return false;

        try
        {
            nlohmann::json request;
            request["type"] = "start";
            request["id"] = id;

            auto response = impl_->pipeClient->SendRequest(request);
            impl_->connected = impl_->pipeClient->IsConnected();
            if (!response.is_object() || !response.value("success", false))
            {
                impl_->lastError = response.value("message", "Unknown error");
                return false;
            }
            return true;
        }
        catch (const std::exception &e)
        {
            impl_->connected = false;
            impl_->lastError = std::string("StartMonitorItem error: ") + e.what();
            return false;
        }
        catch (...)
        {
            impl_->connected = false;
            impl_->lastError = "StartMonitorItem unknown error";
            return false;
        }
    }

    std::vector<MonitorItem> Client::GetAllMonitorItems()
    {
        if (!impl_->connected && !Connect())
            return {};

        try
        {
            nlohmann::json request;
            request["type"] = "list";

            auto response = impl_->pipeClient->SendRequest(request);
            impl_->connected = impl_->pipeClient->IsConnected();
            std::vector<MonitorItem> items;

            if (response.is_object() && response.value("success", false) &&
                response.contains("data") && response["data"].is_array())
            {
                for (const auto &item : response["data"])
                {
                    try
                    {
                        MonitorItem mi;
                        mi.id = item.value("id", "");
                        mi.exePath = item.value("exe_path", "");
                        if (item.contains("args") && !item["args"].is_null())
                        {
                            mi.args = item.value("args", "");
                        }
                        else
                        {
                            mi.args = "";
                        }
                        mi.name = item.value("name", "");
                        mi.minimize = item.value("minimize", false);
                        mi.noWindow = item.value("no_window", false);
                        mi.enabled = item.value("enabled", false);
                        mi.heartbeatTimeoutMs = item.value("heartbeat_timeout_ms", 1000);
                        items.push_back(mi);
                    }
                    catch (const std::exception &e)
                    {
                        impl_->lastError = std::string("Parse item error: ") + e.what();
                        continue;
                    }
                }
            }

            return items;
        }
        catch (const std::exception &e)
        {
            impl_->connected = false;
            impl_->lastError = std::string("GetAllMonitorItems error: ") + e.what();
            return {};
        }
        catch (...)
        {
            impl_->connected = false;
            impl_->lastError = "GetAllMonitorItems unknown error";
            return {};
        }
    }

    ServiceStatus Client::GetServiceStatus()
    {
        if (!impl_->connected && !Connect())
            return {};

        try
        {
            nlohmann::json request;
            request["type"] = "status";

            auto response = impl_->pipeClient->SendRequest(request);
            impl_->connected = impl_->pipeClient->IsConnected();
            ServiceStatus status;

            if (response.is_object() && response.value("success", false) && response.contains("data"))
            {
                try
                {
                    const auto &data = response["data"];
                    status.serviceRunning = data.value("service_running", false);
                    status.totalItems = data.value("total_items", 0);

                    if (data.contains("items") && data["items"].is_array())
                    {
                        for (const auto &item : data["items"])
                        {
                            try
                            {
                                ProcessStatus ps;
                                ps.id = item.value("id", "");
                                ps.name = item.value("name", "");
                                ps.exePath = item.value("exe_path", "");
                                ps.enabled = item.value("enabled", false);
                                if (item.contains("process_id") && !item["process_id"].is_null())
                                {
                                    ps.processId = item.value("process_id", 0);
                                }
                                else
                                {
                                    ps.processId = 0;
                                }
                                ps.lastHeartbeatMs = item.value("last_heartbeat_ms", 0);
                                ps.heartbeatTimeoutMs = item.value("heartbeat_timeout_ms", 1000);
                                ps.restartCount = item.value("restart_count", 0);
                                ps.isAlive = item.value("is_alive", false);
                                ps.isHeartbeatOk = item.value("is_heartbeat_ok", false);
                                status.items.push_back(ps);
                            }
                            catch (const std::exception &e)
                            {
                                impl_->lastError = std::string("Parse process status error: ") + e.what();
                                continue;
                            }
                        }
                    }
                }
                catch (const std::exception &e)
                {
                    impl_->lastError = std::string("Parse status error: ") + e.what();
                }
            }

            return status;
        }
        catch (const std::exception &e)
        {
            impl_->connected = false;
            impl_->lastError = std::string("GetServiceStatus error: ") + e.what();
            return {};
        }
        catch (...)
        {
            impl_->connected = false;
            impl_->lastError = "GetServiceStatus unknown error";
            return {};
        }
    }

    bool Client::SendHeartbeat(const std::string &itemId)
    {
        if (!impl_->connected && !Connect())
            return false;

        try
        {
            nlohmann::json request;
            request["type"] = "heartbeat";
            request["item_id"] = itemId;
            request["timestamp"] = std::chrono::duration_cast<std::chrono::milliseconds>(
                                       std::chrono::system_clock::now().time_since_epoch())
                                       .count();

            auto response = impl_->pipeClient->SendRequest(request);
            impl_->connected = impl_->pipeClient->IsConnected();
            bool success = response.is_object() && response.value("success", false);

            if (!success)
            {
                impl_->lastError = "Heartbeat failed: " + response.value("message", "Unknown error");
                if (impl_->heartbeatFailedCallback)
                    impl_->heartbeatFailedCallback(itemId);
            }

            return success;
        }
        catch (const std::exception &e)
        {
            impl_->connected = false;
            impl_->lastError = std::string("SendHeartbeat error: ") + e.what();
            return false;
        }
        catch (...)
        {
            impl_->connected = false;
            impl_->lastError = "SendHeartbeat unknown error";
            return false;
        }
    }

    void Client::StartHeartbeatThread(const std::string &itemId, int intervalMs)
    {
        std::lock_guard<std::mutex> lock(impl_->heartbeatMutex);

        if (impl_->heartbeatThreads.find(itemId) != impl_->heartbeatThreads.end())
            return;

        impl_->heartbeatRunning[itemId] = true;
        impl_->heartbeatThreads[itemId] = std::make_unique<std::thread>([this, itemId, intervalMs]()
                                                                        {
        while (impl_->heartbeatRunning[itemId]) {
            SendHeartbeat(itemId);
            std::this_thread::sleep_for(std::chrono::milliseconds(intervalMs));
        } });
    }

    void Client::StopHeartbeatThread(const std::string &itemId)
    {
        std::lock_guard<std::mutex> lock(impl_->heartbeatMutex);

        auto it = impl_->heartbeatThreads.find(itemId);
        if (it != impl_->heartbeatThreads.end())
        {
            impl_->heartbeatRunning[itemId] = false;
            if (it->second && it->second->joinable())
                it->second->join();
            impl_->heartbeatThreads.erase(it);
            impl_->heartbeatRunning.erase(itemId);
        }
    }

    void Client::StopAllHeartbeatThreads()
    {
        std::lock_guard<std::mutex> lock(impl_->heartbeatMutex);

        for (auto &pair : impl_->heartbeatRunning)
            pair.second = false;
        for (auto &pair : impl_->heartbeatThreads)
        {
            if (pair.second && pair.second->joinable())
                pair.second->join();
        }

        impl_->heartbeatThreads.clear();
        impl_->heartbeatRunning.clear();
    }

    bool Client::EnsureServiceInstalled(const std::string &servicePath)
    {
        return QuickSetup(servicePath);
    }

    bool Client::EnsureServiceRunning()
    {
        return IsServiceInstalled() && (IsServiceRunning() || StartService());
    }

    void Client::SetHeartbeatFailedCallback(std::function<void(const std::string &)> callback)
    {
        impl_->heartbeatFailedCallback = std::move(callback);
    }

    void Client::SetConnectedChangedCallback(std::function<void(bool)> callback)
    {
        impl_->connectedChangedCallback = std::move(callback);
    }

    bool Client::AddSelfMonitor(const std::string &id, int heartbeatTimeoutMs)
    {
        // 确保已连接到服务
        if (!impl_->connected && !Connect())
        {
            impl_->lastError = "Failed to connect to service";
            return false;
        }

        std::string itemId = id.empty() ? ("self-" + std::to_string(std::chrono::duration_cast<std::chrono::milliseconds>(
                                                                        std::chrono::system_clock::now().time_since_epoch())
                                                                        .count()))
                                        : id;

        // 获取当前程序路径
        std::string exePath = GetCurrentExePath();
        if (exePath.empty())
        {
            impl_->lastError = "Failed to get current executable path";
            return false;
        }

        // 提取程序名称（不含扩展名）
        std::string name;
        try
        {
            std::filesystem::path p(exePath);
            name = p.stem().string();
        }
        catch (const std::exception &e)
        {
            // 如果 filesystem 失败，使用传统方法
            size_t lastSlash = exePath.find_last_of("\\/");
            size_t lastDot = exePath.find_last_of('.');
            if (lastSlash != std::string::npos)
            {
                name = (lastDot != std::string::npos && lastDot > lastSlash)
                           ? exePath.substr(lastSlash + 1, lastDot - lastSlash - 1)
                           : exePath.substr(lastSlash + 1);
            }
            else
            {
                name = (lastDot != std::string::npos)
                           ? exePath.substr(0, lastDot)
                           : exePath;
            }
        }

        if (name.empty())
        {
            name = "SelfMonitoredProcess";
        }

        MonitorItem item;
        item.id = itemId;
        item.exePath = exePath;
        item.name = name;
        item.enabled = true;
        item.heartbeatTimeoutMs = heartbeatTimeoutMs;

        if (AddMonitorItem(item))
        {
            impl_->selfMonitorId = itemId;
            return true;
        }
        return false;
    }

    bool Client::RemoveSelfMonitor()
    {
        if (impl_->selfMonitorId.empty())
        {
            impl_->lastError = "Self monitor not set";
            return false;
        }
        return RemoveMonitorItem(impl_->selfMonitorId);
    }

    bool Client::PauseSelfMonitor()
    {
        if (impl_->selfMonitorId.empty())
        {
            impl_->lastError = "Self monitor not set";
            return false;
        }
        return PauseMonitorItem(impl_->selfMonitorId);
    }

    bool Client::ResumeSelfMonitor()
    {
        if (impl_->selfMonitorId.empty())
        {
            impl_->lastError = "Self monitor not set";
            return false;
        }
        return ResumeMonitorItem(impl_->selfMonitorId);
    }

    bool Client::StartSelfHeartbeat(int intervalMs)
    {
        if (impl_->selfMonitorId.empty())
        {
            return false;
        }
        StartHeartbeatThread(impl_->selfMonitorId, intervalMs);
        return true;
    }

    void Client::SetSelfMonitorId(const std::string &id)
    {
        impl_->selfMonitorId = id;
    }

    std::string Client::GetSelfMonitorId() const
    {
        return impl_->selfMonitorId;
    }

    void Client::StopSelfHeartbeat()
    {
        if (!impl_->selfMonitorId.empty())
        {
            StopHeartbeatThread(impl_->selfMonitorId);
        }
    }

}
