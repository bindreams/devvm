// Spike only (never merged). Stage `e2ctl`: a stdin-to-stdout copy that calls ReadFile and WriteFile on the standard
// handles itself (no CLR console stream, so none of its pre-read wait on a pipe handle), until ReadFile reports the
// pipe's end. C# 5, as the guest's Windows PowerShell compiles it.
using System;
using System.Runtime.InteropServices;

public static class ReadFileCat
{
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr GetStdHandle(int which);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool ReadFile(IntPtr file, byte[] buffer, int count, out int read, IntPtr overlapped);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool WriteFile(IntPtr file, byte[] buffer, int count, out int written, IntPtr overlapped);

    const int ERROR_BROKEN_PIPE = 109;

    public static int Main()
    {
        IntPtr input = GetStdHandle(-10);
        IntPtr output = GetStdHandle(-11);
        byte[] buffer = new byte[65536];
        while (true)
        {
            int read;
            if (!ReadFile(input, buffer, buffer.Length, out read, IntPtr.Zero))
            {
                int error = Marshal.GetLastWin32Error();
                if (error == ERROR_BROKEN_PIPE) return 0;
                Console.Error.WriteLine("ReadFile failed: " + error);
                return 2;
            }
            if (read == 0) return 0;
            int at = 0;
            while (at < read)
            {
                byte[] part = buffer;
                int written;
                if (at > 0) { part = new byte[read - at]; Array.Copy(buffer, at, part, 0, read - at); }
                if (!WriteFile(output, part, read - at, out written, IntPtr.Zero))
                {
                    Console.Error.WriteLine("WriteFile failed: " + Marshal.GetLastWin32Error());
                    return 3;
                }
                at += written;
            }
        }
    }
}
