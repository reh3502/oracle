using System.ComponentModel;
using System.Runtime.InteropServices;

namespace OracleLauncher;

// Keep the whole process tree tied to this console, including abrupt window close.
internal static class ConsoleLifetime
{
    internal static Action? StopRequested;
    internal static readonly ManualResetEventSlim Stopped = new(false);
    static readonly Handler handler = OnControl;
    static IntPtr job;
    delegate bool Handler(uint control);

    internal static void Initialize()
    {
        job = CreateJobObjectW(IntPtr.Zero, null);
        if (job == IntPtr.Zero) throw new Win32Exception();
        var limits = new ExtendedLimits();
        limits.Basic.Flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if (!SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf<ExtendedLimits>()) ||
            !AssignProcessToJobObject(job, GetCurrentProcess()) ||
            !SetConsoleCtrlHandler(handler, true))
            throw new Win32Exception();
        // The OS closes this non-inherited handle at process exit. Assigning the
        // launcher before spawning children leaves no unsupervised startup gap.
    }
    static bool OnControl(uint control)
    {
        if (control > 6 || control is 3 or 4) return false;
        StopRequested?.Invoke();
        // Windows limits close/logoff/shutdown callbacks to a few seconds.
        // Try graceful shutdown, then the job guarantees no orphan bot remains.
        if (control is 2 or 5 or 6) Stopped.Wait(TimeSpan.FromSeconds(4));
        return true;
    }
    internal static void RequestStopForTest() => OnControl(0);
    [StructLayout(LayoutKind.Sequential)]
    struct BasicLimits
    {
        public long ProcessTime, JobTime;
        public uint Flags;
        public UIntPtr MinimumWorkingSet, MaximumWorkingSet;
        public uint ActiveProcesses;
        public UIntPtr Affinity;
        public uint Priority, SchedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct IoCounters { public ulong ReadOps, WriteOps, OtherOps, ReadBytes, WriteBytes, OtherBytes; }
    [StructLayout(LayoutKind.Sequential)]
    struct ExtendedLimits
    {
        public BasicLimits Basic;
        public IoCounters Io;
        public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObjectW(IntPtr attributes, string? name);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr job, int info, ref ExtendedLimits limits, uint length);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetConsoleCtrlHandler(Handler callback, bool add);
}
