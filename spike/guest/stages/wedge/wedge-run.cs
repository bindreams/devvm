// Spike only (never merged). The guest-local half of stage `wedge`: runs a program the way sshd runs the broker's shim
// (`<exe> -c "<script>"`, stdin, stdout and stderr on pipes) with `input` on stdin, and reports how far the bytes got.
// Compiled by stage-user.ps1 (Add-Type, C# 5 as Windows PowerShell has it). The data is m5helper.cs's `emit mib` pattern.
using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;

public class WedgeResult
{
    public bool Exited;     // the program ended within the bound
    public int ExitCode;
    public long Sent;       // stdin bytes the pipe accepted
    public long Received;   // stdout bytes read
    public long ErrBytes;   // stderr bytes read
    public bool Same;       // no stdout byte differs from the input (so far, when the program did not end)
    public bool Drained;    // both output readers reached the end
    public double Seconds;
    public int Pid;
    public string Out;      // stdout as text, when asked for (small outputs)

    public override string ToString()
    {
        return "exited=" + Exited + " exit=" + ExitCode + " sent=" + Sent + " received=" + Received + " stderr=" + ErrBytes +
            " same=" + Same + " drained=" + Drained + " seconds=" + Seconds.ToString("0.0") + " pid=" + Pid;
    }
}

public static class Wedge
{
    // The standard Windows command-line quoting that sshd's DefaultShellEscapeArguments gives the shell's `-c` argument.
    public static string Quote(string arg)
    {
        if (arg.Length > 0 && arg.IndexOfAny(new char[] { ' ', '\t', '\n', '\v', '"' }) < 0) return arg;
        StringBuilder sb = new StringBuilder("\"");
        int slashes = 0;
        foreach (char c in arg)
        {
            if (c == '\\') { slashes++; }
            else if (c == '"') { sb.Append('\\', slashes * 2 + 1); sb.Append('"'); slashes = 0; }
            else { sb.Append('\\', slashes); slashes = 0; sb.Append(c); }
        }
        sb.Append('\\', slashes * 2);
        sb.Append('"');
        return sb.ToString();
    }

    // `emit mib` writes this for chunk k (m5helper.cs Chunk), one MiB per chunk.
    public static byte[] Pattern(int mib)
    {
        byte[] data = new byte[mib << 20];
        for (int k = 0; k < mib; k++)
            for (int i = 0; i < (1 << 20); i++)
                data[(k << 20) + i] = (byte)((i * 7 + k * 13 + (i >> 8)) & 0xFF);
        return data;
    }

    sealed class State
    {
        public long Sent, Received, ErrBytes;
        public int Mismatch;
        public MemoryStream Captured;
    }

    // Runs `exe -c <script>`. `boundMs` is the failure bound: the program may never end (a hang is the finding), so the
    // result says so, with the byte counts, and the process tree is ended.
    public static WedgeResult Run(string exe, string script, byte[] input, int boundMs, bool capture)
    {
        ProcessStartInfo psi = new ProcessStartInfo(exe, "-c " + Quote(script));
        psi.UseShellExecute = false;
        psi.CreateNoWindow = true;
        psi.RedirectStandardInput = true;
        psi.RedirectStandardOutput = true;
        psi.RedirectStandardError = true;
        State st = new State();
        if (capture) st.Captured = new MemoryStream();
        Stopwatch watch = Stopwatch.StartNew();
        Process p = Process.Start(psi);

        Thread writer = new Thread(delegate ()
        {
            try
            {
                Stream s = p.StandardInput.BaseStream;
                for (int off = 0; off < input.Length; off += 4096)
                {
                    int n = Math.Min(4096, input.Length - off);
                    s.Write(input, off, n);
                    Interlocked.Add(ref st.Sent, n);
                }
                s.Flush();
                s.Close();
            }
            catch (IOException) { }  // the program ended, or the tree was ended on the bound
        });
        Thread reader = new Thread(delegate ()
        {
            byte[] buffer = new byte[65536];
            Stream s = p.StandardOutput.BaseStream;
            int n;
            while ((n = s.Read(buffer, 0, buffer.Length)) > 0)
            {
                long at = Interlocked.Read(ref st.Received);
                if (st.Captured != null) { st.Captured.Write(buffer, 0, n); }
                else
                {
                    if (at + n > input.Length) { Interlocked.Exchange(ref st.Mismatch, 1); }
                    else { for (int i = 0; i < n; i++) { if (buffer[i] != input[at + i]) { Interlocked.Exchange(ref st.Mismatch, 1); break; } } }
                }
                Interlocked.Add(ref st.Received, n);
            }
        });
        Thread errReader = new Thread(delegate ()
        {
            byte[] buffer = new byte[65536];
            Stream s = p.StandardError.BaseStream;
            int n;
            while ((n = s.Read(buffer, 0, buffer.Length)) > 0) { Interlocked.Add(ref st.ErrBytes, n); }
        });
        writer.Start(); reader.Start(); errReader.Start();

        WedgeResult r = new WedgeResult();
        r.Pid = p.Id;
        r.Exited = p.WaitForExit(boundMs);
        if (!r.Exited)
        {
            // Counts as they stand at the bound, before anything is ended.
            r.Sent = Interlocked.Read(ref st.Sent);
            r.Received = Interlocked.Read(ref st.Received);
            r.ErrBytes = Interlocked.Read(ref st.ErrBytes);
            r.Seconds = watch.Elapsed.TotalSeconds;
            Process kill = Process.Start("taskkill.exe", "/T /F /PID " + p.Id);
            kill.WaitForExit();
        }
        else
        {
            r.ExitCode = p.ExitCode;
        }
        // The pipes close when the tree is gone, so the threads end; a grandchild that kept an end open is reported.
        r.Drained = reader.Join(boundMs) & errReader.Join(boundMs);
        writer.Join(boundMs);
        if (r.Exited)
        {
            r.Sent = Interlocked.Read(ref st.Sent);
            r.Received = Interlocked.Read(ref st.Received);
            r.ErrBytes = Interlocked.Read(ref st.ErrBytes);
            r.Seconds = watch.Elapsed.TotalSeconds;
        }
        r.Same = Interlocked.CompareExchange(ref st.Mismatch, 0, 0) == 0;
        if (st.Captured != null && r.Drained) r.Out = Encoding.UTF8.GetString(st.Captured.ToArray());
        return r;
    }
}
