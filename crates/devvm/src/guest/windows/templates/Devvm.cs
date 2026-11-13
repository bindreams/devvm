// The guest's native helper. The first-logon script compiles it once (Windows PowerShell 5.1, so C# 5) into Devvm.dll;
// the elevated helper's runner and the boot task (PowerShell 7) load that assembly.
//
// Caller: a process named by its id and start time, opened once. The start time is compared on that handle and the wait is
// on that handle, so an id that has been reused never answers for the process that was meant.
// Child: the runner's command. It is created suspended, put in a job that is killed when its handle closes, and only then
// runs, so every process of its tree is in the job. Ending the tree is ending the job: no process is looked up by id.
// Disk: flushes a directory to the disk.

using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.Win32.SafeHandles;

namespace Devvm
{
    static class Native
    {
        public const uint Synchronize = 0x00100000;
        public const uint ProcessQueryLimitedInformation = 0x1000;
        public const uint InfiniteWait = 0xFFFFFFFF;
        public const uint CreateSuspended = 0x4;
        public const uint CreateNoWindow = 0x08000000;
        public const int StartfUseStdHandles = 0x100;
        public const int HandleFlagInherit = 1;
        public const int ErrorInvalidParameter = 87;
        public const uint GenericWrite = 0x40000000;
        public const uint ShareAll = 7;
        public const uint OpenExisting = 3;
        public const uint FileFlagBackupSemantics = 0x02000000;

        [StructLayout(LayoutKind.Sequential)]
        public struct SecurityAttributes
        {
            public int Length;
            public IntPtr Descriptor;
            public int InheritHandle;
        }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        public struct StartupInfo
        {
            public int Size;
            public string Reserved;
            public string Desktop;
            public string Title;
            public int X;
            public int Y;
            public int XSize;
            public int YSize;
            public int XCountChars;
            public int YCountChars;
            public int FillAttribute;
            public int Flags;
            public short ShowWindow;
            public short Reserved2Size;
            public IntPtr Reserved2;
            public IntPtr StdInput;
            public IntPtr StdOutput;
            public IntPtr StdError;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct ProcessInformation
        {
            public IntPtr Process;
            public IntPtr Thread;
            public int ProcessId;
            public int ThreadId;
        }

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern IntPtr OpenProcess(uint access, bool inherit, int id);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool GetProcessTimes(IntPtr process, out long creation, out long exit, out long kernel, out long user);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool GetExitCodeProcess(IntPtr process, out uint code);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool CloseHandle(IntPtr handle);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool CreatePipe(out IntPtr read, out IntPtr write, ref SecurityAttributes attributes, int size);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool SetHandleInformation(IntPtr handle, int mask, int flags);

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        public static extern bool CreateProcessW(string application, StringBuilder commandLine, IntPtr processAttributes,
            IntPtr threadAttributes, bool inheritHandles, uint flags, IntPtr environment, string directory,
            ref StartupInfo startup, out ProcessInformation process);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern uint ResumeThread(IntPtr thread);

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        public static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool SetInformationJobObject(IntPtr job, int infoClass, IntPtr info, int length);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool TerminateJobObject(IntPtr job, uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool TerminateProcess(IntPtr process, uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        public static extern IntPtr CreateFileW(string path, uint access, uint share, IntPtr attributes, uint disposition,
            uint flags, IntPtr template);

        [DllImport("kernel32.dll", SetLastError = true)]
        public static extern bool FlushFileBuffers(IntPtr handle);

        public static Win32Exception Fail(string what)
        {
            return new Win32Exception(Marshal.GetLastWin32Error(), what + " failed");
        }

        public static void Close(ref IntPtr handle)
        {
            if (handle != IntPtr.Zero)
            {
                CloseHandle(handle);
                handle = IntPtr.Zero;
            }
        }
    }

    // Completes `Done` when the object behind `handle` is signalled (a process has ended). It never closes the handle.
    sealed class Watch : IDisposable
    {
        readonly TaskCompletionSource<bool> done = new TaskCompletionSource<bool>();
        readonly ManualResetEvent signal = new ManualResetEvent(false);
        readonly RegisteredWaitHandle registration;

        public Watch(IntPtr handle)
        {
            signal.SafeWaitHandle = new SafeWaitHandle(handle, false);
            registration = ThreadPool.RegisterWaitForSingleObject(signal, Signalled, null, -1, true);
        }

        void Signalled(object state, bool timedOut)
        {
            done.TrySetResult(true);
        }

        public Task Done
        {
            get { return done.Task; }
        }

        public void Dispose()
        {
            registration.Unregister(null);
            signal.Dispose();
        }
    }

    public sealed class Caller : IDisposable
    {
        IntPtr handle;
        Watch watch;

        Caller()
        {
        }

        // The process with this id, if it started at `startTicks` (UTC ticks, as the caller wrote them); null when there is
        // no such process or the id now belongs to another one.
        public static Caller Open(int id, long startTicks)
        {
            IntPtr handle = Native.OpenProcess(Native.Synchronize | Native.ProcessQueryLimitedInformation, false, id);
            if (handle == IntPtr.Zero)
            {
                if (Marshal.GetLastWin32Error() == Native.ErrorInvalidParameter) return null;
                throw Native.Fail("OpenProcess");
            }
            try
            {
                long creation, exit, kernel, user;
                if (!Native.GetProcessTimes(handle, out creation, out exit, out kernel, out user))
                    throw Native.Fail("GetProcessTimes");
                if (DateTime.FromFileTime(creation).ToUniversalTime().Ticks != startTicks)
                {
                    Native.CloseHandle(handle);
                    return null;
                }
                Caller caller = new Caller();
                caller.handle = handle;
                caller.watch = new Watch(handle);
                return caller;
            }
            catch
            {
                Native.CloseHandle(handle);
                throw;
            }
        }

        public Task Exited
        {
            get { return watch.Done; }
        }

        public void Dispose()
        {
            if (watch != null) watch.Dispose();
            watch = null;
            Native.Close(ref handle);
        }
    }

    public sealed class Child : IDisposable
    {
        const int ExtendedLimitInformation = 9;
        const int ExtendedLimitInformationSize = 144;
        const int LimitFlagsOffset = 16;
        const int KillOnJobClose = 0x2000;

        IntPtr job;
        IntPtr process;
        IntPtr thread;
        bool inJob;
        Watch watch;

        // The child's standard output and error, as the pipes it writes to. Its standard input is at end of file.
        public FileStream Output;
        public FileStream Error;

        Child()
        {
        }

        // Starts `commandLine` (which begins with the quoted `application`) in a job of its own.
        public static Child Start(string application, string commandLine)
        {
            Child child = new Child();
            try
            {
                child.Create(application, commandLine);
                return child;
            }
            catch
            {
                child.Dispose();
                throw;
            }
        }

        void Create(string application, string commandLine)
        {
            job = Native.CreateJobObjectW(IntPtr.Zero, null);
            if (job == IntPtr.Zero) throw Native.Fail("CreateJobObject");
            IntPtr limits = Marshal.AllocHGlobal(ExtendedLimitInformationSize);
            try
            {
                for (int i = 0; i < ExtendedLimitInformationSize; i++) Marshal.WriteByte(limits, i, 0);
                Marshal.WriteInt32(limits, LimitFlagsOffset, KillOnJobClose);
                if (!Native.SetInformationJobObject(job, ExtendedLimitInformation, limits, ExtendedLimitInformationSize))
                    throw Native.Fail("SetInformationJobObject");
            }
            finally
            {
                Marshal.FreeHGlobal(limits);
            }

            IntPtr inRead = IntPtr.Zero, inWrite = IntPtr.Zero;
            IntPtr outRead = IntPtr.Zero, outWrite = IntPtr.Zero;
            IntPtr errRead = IntPtr.Zero, errWrite = IntPtr.Zero;
            try
            {
                Native.SecurityAttributes inherit = new Native.SecurityAttributes();
                inherit.Length = Marshal.SizeOf(typeof(Native.SecurityAttributes));
                inherit.InheritHandle = 1;
                if (!Native.CreatePipe(out inRead, out inWrite, ref inherit, 0)) throw Native.Fail("CreatePipe");
                if (!Native.CreatePipe(out outRead, out outWrite, ref inherit, 0)) throw Native.Fail("CreatePipe");
                if (!Native.CreatePipe(out errRead, out errWrite, ref inherit, 0)) throw Native.Fail("CreatePipe");
                // The ends that stay here are not inherited.
                foreach (IntPtr ours in new IntPtr[] { inWrite, outRead, errRead })
                {
                    if (!Native.SetHandleInformation(ours, Native.HandleFlagInherit, 0))
                        throw Native.Fail("SetHandleInformation");
                }

                Native.StartupInfo startup = new Native.StartupInfo();
                startup.Size = Marshal.SizeOf(typeof(Native.StartupInfo));
                startup.Flags = Native.StartfUseStdHandles;
                startup.StdInput = inRead;
                startup.StdOutput = outWrite;
                startup.StdError = errWrite;
                Native.ProcessInformation created;
                StringBuilder line = new StringBuilder(commandLine);
                if (!Native.CreateProcessW(application, line, IntPtr.Zero, IntPtr.Zero, true,
                    Native.CreateSuspended | Native.CreateNoWindow, IntPtr.Zero, null, ref startup, out created))
                    throw Native.Fail("CreateProcess");
                process = created.Process;
                thread = created.Thread;
                // Suspended until it is in the job: nothing it starts can be outside.
                if (!Native.AssignProcessToJobObject(job, process)) throw Native.Fail("AssignProcessToJobObject");
                inJob = true;
                watch = new Watch(process);
                if (Native.ResumeThread(thread) == 0xFFFFFFFF) throw Native.Fail("ResumeThread");

                // The child has its ends. Closing ours of the input pipe is the child's end of file.
                Native.Close(ref inRead);
                Native.Close(ref inWrite);
                Native.Close(ref outWrite);
                Native.Close(ref errWrite);
                SafeFileHandle output = new SafeFileHandle(outRead, true);
                outRead = IntPtr.Zero;
                Output = new FileStream(output, FileAccess.Read, 1, false);
                SafeFileHandle error = new SafeFileHandle(errRead, true);
                errRead = IntPtr.Zero;
                Error = new FileStream(error, FileAccess.Read, 1, false);
            }
            finally
            {
                Native.Close(ref inRead);
                Native.Close(ref inWrite);
                Native.Close(ref outRead);
                Native.Close(ref outWrite);
                Native.Close(ref errRead);
                Native.Close(ref errWrite);
            }
        }

        // Completes when the child itself has ended.
        public Task Exited
        {
            get { return watch.Done; }
        }

        public int ExitCode
        {
            get
            {
                uint code;
                if (!Native.GetExitCodeProcess(process, out code)) throw Native.Fail("GetExitCodeProcess");
                return unchecked((int)code);
            }
        }

        // Ends every process of the tree, at once.
        public void Terminate()
        {
            if (!Native.TerminateJobObject(job, 1)) throw Native.Fail("TerminateJobObject");
        }

        // Closing the job ends whatever is still in it. A child that never got into it is ended by its own handle.
        public void Dispose()
        {
            if (watch != null) watch.Dispose();
            watch = null;
            if (Output != null) Output.Dispose();
            if (Error != null) Error.Dispose();
            if (process != IntPtr.Zero && !inJob) Native.TerminateProcess(process, 1);
            Native.Close(ref thread);
            Native.Close(ref process);
            Native.Close(ref job);
        }
    }

    public static class Disk
    {
        // Flushes a directory's entries to the disk: a rename in it is on the disk when this returns.
        public static void FlushDirectory(string path)
        {
            IntPtr handle = Native.CreateFileW(path, Native.GenericWrite, Native.ShareAll, IntPtr.Zero, Native.OpenExisting,
                Native.FileFlagBackupSemantics, IntPtr.Zero);
            if (handle == new IntPtr(-1)) throw Native.Fail("CreateFile of " + path);
            try
            {
                if (!Native.FlushFileBuffers(handle)) throw Native.Fail("FlushFileBuffers of " + path);
            }
            finally
            {
                Native.CloseHandle(handle);
            }
        }
    }
}
