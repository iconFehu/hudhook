// A harmless DLL used to exercise the injector's optional callback protocol.
#include <windows.h>

static HMODULE self;
static LONG active;

static void record(const char *message, DWORD length) {
    char path[MAX_PATH];
    GetModuleFileNameA(self, path, MAX_PATH);
    DWORD index = lstrlenA(path);
    while (index && path[index - 1] != '\\') --index;
    lstrcpyA(path + index, "smoke.log");
    HANDLE file = CreateFileA(path, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                             NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file != INVALID_HANDLE_VALUE) {
        SetFilePointer(file, 0, NULL, FILE_END);
        DWORD written;
        WriteFile(file, message, length, &written, NULL);
        CloseHandle(file);
    }
}

BOOL WINAPI DllMain(HMODULE module, DWORD reason, LPVOID reserved) {
    (void)reserved;
    self = module;
    if (reason == DLL_PROCESS_ATTACH) record("ATTACH\n", 7);
    if (reason == DLL_PROCESS_DETACH) record("DETACH\n", 7);
    return TRUE;
}

#ifndef NO_CALLBACK
#ifdef _M_IX86
#pragma comment(linker, "/EXPORT:L4D2_RequestResume=_L4D2_RequestResume@4")
#pragma comment(linker, "/EXPORT:RequestStop=_RequestStop@4")
#pragma comment(linker, "/EXPORT:RequestReject=_RequestReject@4")
#pragma comment(linker, "/EXPORT:RequestUnload=_RequestUnload@4")
#endif

__declspec(dllexport) DWORD WINAPI L4D2_RequestResume(LPVOID argument) {
    (void)argument;
    record("RESUME\n", 7);
    if (InterlockedExchange(&active, 1)) record("ALREADY_ACTIVE\n", 15);
    else record("INIT\n", 5);
    return 1;
}

__declspec(dllexport) DWORD WINAPI RequestStop(LPVOID argument) {
    (void)argument;
    InterlockedExchange(&active, 0);
    record("STOP\n", 5);
    return 1;
}

__declspec(dllexport) DWORD WINAPI RequestReject(LPVOID argument) {
    (void)argument;
    return 0;
}

__declspec(dllexport) DWORD WINAPI RequestUnload(LPVOID argument) {
    (void)argument;
    record("UNLOAD\n", 7);
    FreeLibraryAndExitThread(self, 1);
    return 0;
}
#endif
