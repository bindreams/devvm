// Spike only (never merged). Stage `e2ctl`: the E2 control. A child program is given its three standard streams as pipe
// pairs laid out the way OpenSSH's fileio_pipe makes them (a named pipe, one instance, 4,096-byte buffers: the child
// reads the client end of the stdin pipe and writes the server end of the stdout and stderr pipes, the parent keeps the
// other ends, overlapped), with no sshd in the path. The layout of the CHILD's ends is the variable: `ovl` is OpenSSH's
// (FILE_FLAG_OVERLAPPED), `sync` is what CreatePipe gives (not overlapped). 1 MiB goes to the child's stdin in 32 KiB
// writes; the stdin end is closed right after the last write (`early`) or only after the whole echo came back (`late`,
// E2). A sample that does not end in <bound> seconds is a HANG: the child is killed (the process this program started).
// C# 5, as the guest's Windows PowerShell compiles it.
//
// Usage: pipectl.exe <reader> <ovl|sync> <early|late> <samples> <bound seconds>
//   readers: emit      emit.exe cat (a .NET Framework console program; the wedge stage's sink); echo expected
//            readfile  readfile.exe: ReadFile and WriteFile on the standard handles, no CLR console stream; echo expected
//            ps5       powershell.exe -c "[Console]::In.ReadToEnd().Length"; the character count expected (ASCII input)
// Output: one line per sample, `<reader> <layout> <eof> #n: PASS|HANG|FAIL ...`; the samples of a cell stop at the first
// HANG or FAIL. The number of samples is the sample size of the measurement, not a retry limit.
using System;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Microsoft.Win32.SafeHandles;

public static class PipeCtl
{
    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_ATTRIBUTES { public int nLength; public IntPtr lpSecurityDescriptor; public int bInheritHandle; }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO
    {
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX; public int dwY; public int dwXSize; public int dwYSize; public int dwXCountChars; public int dwYCountChars;
        public int dwFillAttribute; public int dwFlags; public short wShowWindow; public short cbReserved2;
        public IntPtr lpReserved2; public IntPtr hStdInput; public IntPtr hStdOutput; public IntPtr hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION { public IntPtr hProcess; public IntPtr hThread; public int dwProcessId; public int dwThreadId; }

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateNamedPipe(string name, uint openMode, uint pipeMode, uint maxInstances, uint outSize, uint inSize, uint timeout, ref SECURITY_ATTRIBUTES sa);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFile(string name, uint access, uint share, ref SECURITY_ATTRIBUTES sa, uint disposition, uint flags, IntPtr template);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcess(string application, StringBuilder commandLine, IntPtr processAttributes, IntPtr threadAttributes, bool inheritHandles,
        uint flags, IntPtr environment, string directory, ref STARTUPINFO startup, out PROCESS_INFORMATION info);

    [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr process, out uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr handle);

    const uint PIPE_ACCESS_OUTBOUND = 0x2;
    const uint FILE_FLAG_OVERLAPPED = 0x40000000;
    const uint GENERIC_READ = 0x80000000;
    const uint OPEN_EXISTING = 3;
    const uint FILE_ATTRIBUTE_NORMAL = 0x80;
    const uint CREATE_NO_WINDOW = 0x08000000;
    const int STARTF_USESTDHANDLES = 0x100;
    const uint WAIT_OBJECT_0 = 0;
    static readonly IntPtr Invalid = new IntPtr(-1);
    static int counter;

    static SECURITY_ATTRIBUTES Attributes(bool inherit)
    {
        SECURITY_ATTRIBUTES sa = new SECURITY_ATTRIBUTES();
        sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
        sa.bInheritHandle = inherit ? 1 : 0;
        return sa;
    }

    // childReads: the child reads (stdin). The server end is outbound, as in fileio_pipe: the stdin pipe's server end is
    // the parent's, the stdout and stderr pipes' server end is the child's. The parent's ends are always overlapped.
    static void MakePipe(bool childReads, bool childOverlapped, out IntPtr parentEnd, out IntPtr childEnd)
    {
        string name = "\\\\.\\pipe\\devvm-e2ctl." + Process.GetCurrentProcess().Id + "." + Interlocked.Increment(ref counter);
        bool serverIsChild = !childReads;
        SECURITY_ATTRIBUTES serverSa = Attributes(serverIsChild);
        bool serverOverlapped = serverIsChild ? childOverlapped : true;
        IntPtr server = CreateNamedPipe(name, PIPE_ACCESS_OUTBOUND | (serverOverlapped ? FILE_FLAG_OVERLAPPED : 0), 0, 1, 4096, 4096, 0, ref serverSa);
        if (server == Invalid) throw new Exception("CreateNamedPipe failed: " + Marshal.GetLastWin32Error());
        SECURITY_ATTRIBUTES clientSa = Attributes(!serverIsChild);
        bool clientOverlapped = serverIsChild ? true : childOverlapped;
        IntPtr client = CreateFile(name, GENERIC_READ, 0, ref clientSa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL | (clientOverlapped ? FILE_FLAG_OVERLAPPED : 0), IntPtr.Zero);
        if (client == Invalid) throw new Exception("CreateFile failed: " + Marshal.GetLastWin32Error());
        parentEnd = serverIsChild ? client : server;
        childEnd = serverIsChild ? server : client;
    }

    static byte[] Payload(bool ascii)
    {
        byte[] data = new byte[1 << 20];
        for (int i = 0; i < data.Length; i++)
            data[i] = ascii ? (byte)((i % 80 == 79) ? '\n' : 'a' + (i % 26)) : (byte)((i * 7 + (i >> 8)) & 0xFF);
        return data;
    }

    static string CommandLine(string reader, out bool echo)
    {
        echo = true;
        switch (reader)
        {
            case "emit": return "\"C:\\ProgramData\\devvm\\m5\\bin\\emit.exe\" cat";
            case "readfile": return "\"C:\\ProgramData\\devvm\\e2ctl\\readfile.exe\"";
            case "ps5":
                echo = false;
                return "\"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" -NoProfile -NonInteractive -c \"[Console]::In.ReadToEnd().Length\"";
        }
        throw new ArgumentException("unknown reader " + reader);
    }

    static void Say(string text) { Console.WriteLine(text); Console.Out.Flush(); }

    // One sample's state, shared by its threads under Gate.
    static readonly object Gate = new object();
    static long echoed;
    static long sent;
    static bool finished;

    static string Sample(string reader, string layout, string eof, int n, int boundSeconds)
    {
        bool overlapped = layout == "ovl";
        bool late = eof == "late";
        bool echo;
        string command = CommandLine(reader, out echo);
        byte[] payload = Payload(!echo);
        lock (Gate) { echoed = 0; sent = 0; finished = false; }
        DateTime began = DateTime.UtcNow;

        IntPtr inParent, inChild, outParent, outChild, errParent, errChild;
        MakePipe(true, overlapped, out inParent, out inChild);
        MakePipe(false, overlapped, out outParent, out outChild);
        MakePipe(false, overlapped, out errParent, out errChild);

        STARTUPINFO si = new STARTUPINFO();
        si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdInput = inChild; si.hStdOutput = outChild; si.hStdError = errChild;
        PROCESS_INFORMATION pi;
        if (!CreateProcess(null, new StringBuilder(command), IntPtr.Zero, IntPtr.Zero, true, CREATE_NO_WINDOW, IntPtr.Zero, null, ref si, out pi))
            throw new Exception("CreateProcess failed: " + Marshal.GetLastWin32Error());
        CloseHandle(pi.hThread);
        CloseHandle(inChild); CloseHandle(outChild); CloseHandle(errChild);

        MemoryStream collected = new MemoryStream();
        FileStream stdin = new FileStream(new SafeFileHandle(inParent, true), FileAccess.Write, 4096, true);
        FileStream stdout = new FileStream(new SafeFileHandle(outParent, true), FileAccess.Read, 4096, true);
        FileStream stderr = new FileStream(new SafeFileHandle(errParent, true), FileAccess.Read, 4096, true);

        Thread outReader = new Thread(delegate ()
        {
            byte[] buffer = new byte[65536];
            try
            {
                int count;
                while ((count = stdout.Read(buffer, 0, buffer.Length)) > 0)
                {
                    lock (Gate)
                    {
                        collected.Write(buffer, 0, count);
                        echoed += count;
                        Monitor.PulseAll(Gate);
                    }
                }
            }
            catch (IOException) { }
        });
        long errBytes = 0;
        Thread errReader = new Thread(delegate ()
        {
            byte[] buffer = new byte[4096];
            try
            {
                int count;
                while ((count = stderr.Read(buffer, 0, buffer.Length)) > 0) Interlocked.Add(ref errBytes, count);
            }
            catch (IOException) { }
        });
        Thread writer = new Thread(delegate ()
        {
            try
            {
                for (int at = 0; at < payload.Length; at += 32768)
                {
                    int count = Math.Min(32768, payload.Length - at);
                    stdin.Write(payload, at, count);
                    lock (Gate) { sent = at + count; }
                }
                if (late)
                {
                    // The wait re-checks the real condition; the bound below ends the child, which sets `finished`.
                    lock (Gate) { while (echoed < payload.Length && !finished) Monitor.Wait(Gate); }
                }
                stdin.Close();
            }
            catch (IOException) { }
            catch (ObjectDisposedException) { }
        });
        outReader.Start(); errReader.Start(); writer.Start();

        // The failure bound for an external event that may never come: a child that never ends.
        bool hung = WaitForSingleObject(pi.hProcess, (uint)(boundSeconds * 1000)) != WAIT_OBJECT_0;
        long sentAtEnd, echoedAtEnd;
        lock (Gate) { sentAtEnd = sent; echoedAtEnd = echoed; }
        if (hung) TerminateProcess(pi.hProcess, 1);
        WaitForSingleObject(pi.hProcess, 0xFFFFFFFF);
        lock (Gate) { finished = true; Monitor.PulseAll(Gate); }
        outReader.Join(); errReader.Join(); writer.Join();
        uint exitCode;
        GetExitCodeProcess(pi.hProcess, out exitCode);
        CloseHandle(pi.hProcess);
        stdout.Dispose(); stderr.Dispose(); stdin.Dispose();

        byte[] output = collected.ToArray();
        bool same;
        string shown;
        if (echo)
        {
            same = output.Length == payload.Length;
            for (int i = 0; same && i < output.Length; i++) same = output[i] == payload[i];
            shown = "stdout=" + output.Length + "/" + payload.Length;
        }
        else
        {
            string text = Encoding.ASCII.GetString(output).Trim();
            same = text == payload.Length.ToString();
            shown = "stdout=[" + text + "]";
        }
        double took = (DateTime.UtcNow - began).TotalSeconds;
        string verdict = hung ? "HANG" : (same && exitCode == 0) ? "PASS" : "FAIL";
        string state = hung ? " at the bound: sent=" + sentAtEnd + "/" + payload.Length + " echoed=" + echoedAtEnd : "";
        Say(reader + " " + layout + " " + eof + " #" + n + ": " + verdict + " exit=" + exitCode + " " + shown + " stderr=" + errBytes + " took=" + took.ToString("F1") + "s" + state);
        return verdict;
    }

    public static int Main(string[] args)
    {
        string reader = args[0], layout = args[1], eof = args[2];
        int samples = int.Parse(args[3]), bound = int.Parse(args[4]);
        for (int n = 1; n <= samples; n++)
        {
            if (Sample(reader, layout, eof, n, bound) != "PASS") return 1;
        }
        return 0;
    }
}
