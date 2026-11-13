// Spike only (never merged). Runs in the user's session (session 1), started by the task `devvm-winwatch` (a logon
// trigger and the stage script), compiled as a windows application so that it has no console of its own. Every second it
// lists the top-level windows of its desktop; when the list changes it appends the list to
// C:\ProgramData\devvm\win\windows.log: every visible window, and every window (visible or not) of a console-ish process.
// After each sample it writes the UTC ticks of the sample's start to C:\ProgramData\devvm\win\beat.txt: what the host
// waits on to know that the log holds everything up to a moment it has noted.
// Whether the agent's console window shows is the question (the Windows Terminal window of run 43289660404, screen 3).
// C# 5 only: the guest compiles it with Windows PowerShell's Add-Type.
using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class WinWatch
{
    delegate bool EnumProc(IntPtr hwnd, IntPtr lparam);

    [StructLayout(LayoutKind.Sequential)]
    struct RECT { public int Left, Top, Right, Bottom; }

    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc proc, IntPtr lparam);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int max);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd, StringBuilder text, int max);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")] static extern IntPtr GetWindowLongPtr(IntPtr hwnd, int index);
    [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr hwnd, uint command);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr hwnd, int attribute, out int value, int size);
    [DllImport("kernel32.dll")] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder name, ref uint size);

    const int GWL_EXSTYLE = -20;
    const uint GW_OWNER = 4;
    const int DWMWA_CLOAKED = 14;
    const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;

    static readonly string[] ConsoleIsh =
        { "ssh-broker.exe", "conhost.exe", "openconsole.exe", "windowsterminal.exe", "wt.exe", "pwsh.exe", "powershell.exe", "cmd.exe", "emit.exe" };

    static string ExeOf(uint pid)
    {
        IntPtr process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        if (process == IntPtr.Zero) return "?";
        try
        {
            StringBuilder name = new StringBuilder(1024);
            uint size = (uint)name.Capacity;
            return QueryFullProcessImageName(process, 0, name, ref size) ? Path.GetFileName(name.ToString()) : "?";
        }
        finally { CloseHandle(process); }
    }

    static string Describe(IntPtr hwnd, bool visible, string exe, uint pid)
    {
        StringBuilder title = new StringBuilder(512);
        GetWindowText(hwnd, title, title.Capacity);
        StringBuilder cls = new StringBuilder(256);
        GetClassName(hwnd, cls, cls.Capacity);
        RECT rect;
        GetWindowRect(hwnd, out rect);
        int cloaked;
        if (DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, out cloaked, 4) != 0) cloaked = -1;
        return string.Format("0x{0:X} pid={1} exe={2} vis={3} iconic={4} cloaked={5} exstyle=0x{6:X} owner=0x{7:X} class={8} rect={9},{10},{11},{12} title=\"{13}\"",
            hwnd.ToInt64(), pid, exe, visible ? 1 : 0, IsIconic(hwnd) ? 1 : 0, cloaked, GetWindowLongPtr(hwnd, GWL_EXSTYLE).ToInt64(),
            GetWindow(hwnd, GW_OWNER).ToInt64(), cls, rect.Left, rect.Top, rect.Right, rect.Bottom, title.ToString().Replace("\"", "'"));
    }

    static List<string> Sample()
    {
        List<string> found = new List<string>();
        EnumWindows(delegate(IntPtr hwnd, IntPtr lparam)
        {
            uint pid;
            GetWindowThreadProcessId(hwnd, out pid);
            bool visible = IsWindowVisible(hwnd);
            string exe = ExeOf(pid);
            if (visible || Array.IndexOf(ConsoleIsh, exe.ToLowerInvariant()) >= 0) found.Add(Describe(hwnd, visible, exe, pid));
            return true;
        }, IntPtr.Zero);
        found.Sort(StringComparer.Ordinal);
        return found;
    }

    public static int Main()
    {
        string dir = @"C:\ProgramData\devvm\win";
        Directory.CreateDirectory(dir);
        string log = Path.Combine(dir, "windows.log");
        string beat = Path.Combine(dir, "beat.txt");
        string previous = null;
        long samples = 0;
        while (true)
        {
            samples++;
            long cycle = DateTime.UtcNow.Ticks;
            try
            {
                List<string> now = Sample();
                IntPtr front = GetForegroundWindow();
                string signature = string.Join("\n", now.ToArray()) + "\nforeground=" + front.ToInt64();
                if (signature != previous)
                {
                    previous = signature;
                    StringBuilder block = new StringBuilder();
                    block.AppendFormat("== {0} sample={1} windows={2} foreground=0x{3:X}\n", DateTime.UtcNow.ToString("o"), samples, now.Count, front.ToInt64());
                    foreach (string line in now) block.Append(line).Append('\n');
                    File.AppendAllText(log, block.ToString(), new UTF8Encoding(false));
                }
            }
            catch (Exception error)
            {
                File.AppendAllText(log, "== " + DateTime.UtcNow.ToString("o") + " ERROR " + error.Message.Replace("\n", " ") + "\n");
            }
            try { File.WriteAllText(beat, cycle.ToString()); }
            catch (IOException) { } // the host is reading it; the next sample writes it again
            Thread.Sleep(1000); // the sampling interval, not a wait for a condition
        }
    }
}
