// Spike only (never merged). The programs M5 and M10 run in the guest, compiled once by the stage and copied under the
// names the tests call. By its own file name the program is `emit` (raw bytes, exit codes, cwd, stdin, Ctrl-C) or
// anything else (`argv`, `-x`, `a=b`: it prints its arguments as hex of their UTF-8 bytes, and the raw command line).
// C# 5, as the guest's Windows PowerShell compiles it.
using System;
using System.IO;
using System.Text;
using System.Threading;

public static class M5Helper
{
    static string Hex(string text)
    {
        StringBuilder sb = new StringBuilder();
        foreach (byte b in Encoding.UTF8.GetBytes(text)) sb.Append(b.ToString("x2"));
        return sb.ToString();
    }

    static void Write(Stream stream, string text)
    {
        byte[] bytes = Encoding.UTF8.GetBytes(text);
        stream.Write(bytes, 0, bytes.Length);
        stream.Flush();
    }

    static byte[] Chunk(int k, bool complement)
    {
        byte[] chunk = new byte[1 << 20];
        for (int i = 0; i < chunk.Length; i++)
        {
            byte b = (byte)((i * 7 + k * 13 + (i >> 8)) & 0xFF);
            chunk[i] = complement ? (byte)(255 - b) : b;
        }
        return chunk;
    }

    public static int Main(string[] args)
    {
        string self = Path.GetFileNameWithoutExtension(Environment.GetCommandLineArgs()[0]);
        Stream o = Console.OpenStandardOutput();
        if (self != "emit")
        {
            StringBuilder sb = new StringBuilder();
            sb.Append("ARGC=" + args.Length + "\n");
            for (int i = 0; i < args.Length; i++) sb.Append("ARG" + i + "=" + Hex(args[i]) + "\n");
            sb.Append("CMDLINE=" + Hex(Environment.CommandLine) + "\n");
            Write(o, sb.ToString());
            return 0;
        }
        if (args.Length == 0) return 99;
        Stream e = Console.OpenStandardError();
        switch (args[0])
        {
            case "all256":
                {
                    byte[] up = new byte[256];
                    byte[] down = new byte[256];
                    for (int i = 0; i < 256; i++) { up[i] = (byte)i; down[i] = (byte)(255 - i); }
                    o.Write(up, 0, 256); o.Flush();
                    e.Write(down, 0, 256); e.Flush();
                    return 0;
                }
            case "crlf":
                {
                    byte[] a = Encoding.ASCII.GetBytes("a\r\nb\nc\r\n\r\nlast");
                    byte[] b = Encoding.ASCII.GetBytes("e1\r\ne2\n");
                    o.Write(a, 0, a.Length); o.Flush();
                    e.Write(b, 0, b.Length); e.Flush();
                    return 0;
                }
            case "mib":
                {
                    int n = int.Parse(args[1]);
                    for (int k = 0; k < n; k++)
                    {
                        byte[] x = Chunk(k, false);
                        o.Write(x, 0, x.Length); o.Flush();
                        byte[] y = Chunk(k, true);
                        e.Write(y, 0, y.Length); e.Flush();
                    }
                    return 0;
                }
            case "exit":
                return unchecked((int)long.Parse(args[1]));
            case "cat":
                {
                    Stream i = Console.OpenStandardInput();
                    byte[] buffer = new byte[65536];
                    int read;
                    while ((read = i.Read(buffer, 0, buffer.Length)) > 0) { o.Write(buffer, 0, read); }
                    o.Flush();
                    return 0;
                }
            case "cwd":
                Write(o, "CWD=" + Hex(Environment.CurrentDirectory) + "\n");
                return 0;
            case "wait":
                {
                    Write(o, "STARTED\n");
                    Console.CancelKeyPress += delegate (object sender, ConsoleCancelEventArgs ev)
                    {
                        Write(o, "INTERRUPTED\n");
                        Environment.Exit(130);
                    };
                    Thread.Sleep(Timeout.Infinite);  // the test ends this with Ctrl-C
                    return 0;
                }
        }
        return 98;
    }
}
