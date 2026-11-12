// Spike only (never merged). Stage `wedge`, variant `echo`: a TCP echo server on port 22 (QEMU forwards the host's 2222
// to it) with no sshd, no pipes and no shell in the path. Per connection one thread reads up to 32 KiB and writes it back,
// the loop sshd-session runs between its socket and its child's pipes. C# 5, as the guest's Windows PowerShell compiles it.
using System;
using System.Net;
using System.Net.Sockets;
using System.Threading;

public static class DevvmEcho
{
    static void Serve(TcpClient client)
    {
        try
        {
            NetworkStream stream = client.GetStream();
            byte[] buffer = new byte[32768];
            int count;
            while ((count = stream.Read(buffer, 0, buffer.Length)) > 0) stream.Write(buffer, 0, count);
        }
        catch (Exception) { }
        finally { client.Close(); }
    }

    public static void Main()
    {
        TcpListener listener = new TcpListener(IPAddress.Any, 22);
        listener.Start();
        while (true)
        {
            TcpClient client = listener.AcceptTcpClient();
            Thread thread = new Thread(delegate () { Serve(client); });
            thread.Start();
        }
    }
}
