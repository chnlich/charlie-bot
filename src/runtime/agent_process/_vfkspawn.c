/* vfork-shaped spawn for the piped backend transports.
 *
 * Popen with a preexec_fn forces the kernel's full fork, whose page-table copy
 * scales with the forking process's resident set (~55 us/MB here — seconds at
 * the server's multi-GB RSS). The piped transports only need pdeathsig
 * child-side, so this stub spawns via clone(CLONE_VM|CLONE_VFORK) — no
 * page-table copy — and runs the fixed child sequence (pdeathsig, setsid,
 * stdio dup2, default signal dispositions, close-from-3, execve) as pure
 * syscalls: the child shares the parent's address space, so nothing here may
 * allocate or call back into Python. The parent releases the GIL across the
 * clone-through-report-read wait, so a slow exec of one child never stops the
 * server's other Python threads. Setup and execve failures report the child's
 * errno over a CLOEXEC pipe and exit 127; a successful execve closes that
 * pipe, so the parent reads EOF as success.
 */
#define _GNU_SOURCE
#include <Python.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct spawn_args {
  char **argv;
  char **envp;
  const char *cwd;
  int in_fd;
  int out_fd;
  int err_fd;
  int report_fd;
  pid_t parent_pid;
  long exec_delay_ms; /* test hook: blocks the child before execve */
  sigset_t oldmask;
};

static void report_and_exit(struct spawn_args *a, int err) {
  ssize_t ignored = write(a->report_fd, &err, sizeof(err));
  (void)ignored;
  _exit(127);
}

/* Runs on a private stack while sharing the parent's address space: syscalls
   only, no returns, no allocation — anything else risks the parent's heap. */
static int child_main(void *argp) {
  struct spawn_args *a = argp;
  /* SIGKILL when the spawning process dies; a parent that died between the
     clone and this prctl reparented us to init — detect and exit. */
  if (prctl(PR_SET_PDEATHSIG, SIGKILL, 0UL, 0UL, 0UL) != 0)
    report_and_exit(a, errno);
  if (getppid() != a->parent_pid)
    report_and_exit(a, EPERM);
  if (setsid() < 0 || dup2(a->in_fd, STDIN_FILENO) < 0 || dup2(a->out_fd, STDOUT_FILENO) < 0 ||
      dup2(a->err_fd, STDERR_FILENO) < 0)
    report_and_exit(a, errno);
  /* Popen(restore_signals=True) parity: the exec'd CLI expects the defaults. */
  signal(SIGPIPE, SIG_DFL);
  signal(SIGXFSZ, SIG_DFL);
  /* Everything above the stdio pair goes; the report fd stays for the error
     path and its CLOEXEC flag closes it on the successful execve. */
  if (a->report_fd > 3 && syscall(SYS_close_range, 3U, (unsigned)(a->report_fd - 1), 0U) != 0)
    report_and_exit(a, errno);
  if (syscall(SYS_close_range, (unsigned)(a->report_fd + 1), ~0U, 0U) != 0)
    report_and_exit(a, errno);
  if (a->cwd != NULL && chdir(a->cwd) != 0)
    report_and_exit(a, errno);
  sigprocmask(SIG_SETMASK, &a->oldmask, NULL);
  /* Regression-test hook only: _VFORKSPAWN_TEST_EXEC_DELAY_MS makes the child
     sit before execve so a test can watch the parent's other threads during
     the wait. nanosleep is a pure syscall, so the child path stays
     allocation- and Python-free. */
  if (a->exec_delay_ms > 0) {
    struct timespec ts = {a->exec_delay_ms / 1000, (a->exec_delay_ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
  }
  execve(a->argv[0], a->argv, a->envp);
  report_and_exit(a, errno);
  return 127; /* unreachable; keeps the compiler honest about the return type */
}

static char *copy_utf8(PyObject *value) {
  Py_ssize_t size;
  const char *utf8 = PyUnicode_AsUTF8AndSize(value, &size);
  if (utf8 == NULL)
    return NULL;
  char *copy = PyMem_Malloc((size_t)size + 1);
  if (copy == NULL) {
    PyErr_NoMemory();
    return NULL;
  }
  memcpy(copy, utf8, (size_t)size + 1);
  return copy;
}

static void free_strings(char **strings, Py_ssize_t count) {
  if (strings == NULL)
    return;
  for (Py_ssize_t i = 0; i < count; i++)
    PyMem_Free(strings[i]);
  PyMem_Free(strings);
}

static PyObject *py_spawn(PyObject *self, PyObject *args) {
  PyObject *argv_obj, *env_obj;
  const char *cwd;
  int in_fd, out_fd, err_fd;
  int parent_pid;
  (void)self;
  if (!PyArg_ParseTuple(args, "OOziiii", &argv_obj, &env_obj, &cwd, &in_fd, &out_fd, &err_fd,
                        &parent_pid))
    return NULL;
  Py_ssize_t argc = PyList_Size(argv_obj);
  Py_ssize_t envc = PyList_Size(env_obj);
  if (argc < 1) {
    PyErr_SetString(PyExc_ValueError, "argv must not be empty");
    return NULL;
  }
  /* Copy strings out of the caller's lists before releasing the GIL. The child
     reads stable buffers even if another Python thread mutates those lists. */
  char **argv = PyMem_Calloc((size_t)argc + 1, sizeof(char *));
  char **envp = PyMem_Calloc((size_t)envc + 1, sizeof(char *));
  if (argv == NULL || envp == NULL) {
    free_strings(argv, argc);
    free_strings(envp, envc);
    return PyErr_NoMemory();
  }
  for (Py_ssize_t i = 0; i < argc; i++) {
    PyObject *item = PyList_GetItem(argv_obj, i); /* borrowed */
    if (item == NULL || (argv[i] = copy_utf8(item)) == NULL)
      goto fail;
  }
  for (Py_ssize_t i = 0; i < envc; i++) {
    PyObject *item = PyList_GetItem(env_obj, i); /* borrowed */
    if (item == NULL || (envp[i] = copy_utf8(item)) == NULL)
      goto fail;
  }

  int report[2];
  if (pipe2(report, O_CLOEXEC) != 0) {
    PyErr_SetFromErrno(PyExc_OSError);
    goto fail;
  }
  /* Lift the report write end above the fd sweep the child runs, keeping its
     CLOEXEC flag (F_DUPFD would drop it). */
  int report_w = fcntl(report[1], F_DUPFD_CLOEXEC, 10);
  if (report_w < 0) {
    PyErr_SetFromErrno(PyExc_OSError);
    close(report[0]);
    close(report[1]);
    goto fail;
  }
  close(report[1]);
  void *stack = mmap(NULL, 256 * 1024, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
  if (stack == MAP_FAILED) {
    PyErr_SetFromErrno(PyExc_OSError);
    close(report[0]);
    close(report_w);
    goto fail;
  }
  /* Test hook (see child_main): the delay rides the spawn_args struct, read
     parent-side while the GIL is still held. */
  long exec_delay_ms = 0;
  const char *delay_env = getenv("_VFORKSPAWN_TEST_EXEC_DELAY_MS");
  if (delay_env != NULL)
    exec_delay_ms = strtol(delay_env, NULL, 10);
  struct spawn_args a = {argv,        envp,   cwd,  in_fd,  out_fd, err_fd, report_w,
                         (pid_t)parent_pid, exec_delay_ms, {{0}}};
  sigset_t all;
  sigfillset(&all);
  /* Every Python C-API call above is done: from here through the report-pipe
     read and a failed exec's waitpid the GIL is released, so a child that
     sits before execve holds one worker thread, not the server's other
     Python threads. While the child shares this address space those threads
     run Python against the same heap; child_main uses only its private stack,
     spawn_args, the prebuilt argv/env buffers and syscalls. The strings were
     copied above and stay stable through execve; child_main makes no Python or
     allocator calls. */
  pid_t pid;
  int clone_errno = 0;
  int child_err = 0;
  int exec_failed = 0;
  int read_errno = 0;
  Py_BEGIN_ALLOW_THREADS
  /* The blocked mask is inherited so no handler runs in the child; the child
     restores the saved mask right before execve. */
  pthread_sigmask(SIG_BLOCK, &all, &a.oldmask);
  pid = clone(child_main, (char *)stack + 256 * 1024, CLONE_VM | CLONE_VFORK | SIGCHLD, &a);
  clone_errno = errno;
  pthread_sigmask(SIG_SETMASK, &a.oldmask, NULL);
  munmap(stack, 256 * 1024);
  close(report_w);
  if (pid < 0) {
    close(report[0]);
  } else {
    /* The child has execed or exited here (CLONE_VFORK), so its argv/envp
       reads are done and the arrays can go back once the GIL returns. */
    ssize_t got = read(report[0], &child_err, sizeof(child_err));
    close(report[0]);
    if (got == (ssize_t)sizeof(child_err)) {
      exec_failed = 1;
      int status;
      while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {
      }
    } else if (got != 0) {
      read_errno = got < 0 ? errno : EIO;
    }
  }
  Py_END_ALLOW_THREADS
  if (pid < 0) {
    errno = clone_errno;
    PyErr_SetFromErrno(PyExc_OSError);
    goto fail;
  }
  if (exec_failed) {
    errno = child_err;
    PyErr_SetFromErrnoWithFilename(PyExc_OSError, argv[0]);
    goto fail;
  }
  if (read_errno != 0) {
    errno = read_errno;
    PyErr_SetFromErrno(PyExc_OSError);
    goto fail;
  }
  free_strings(argv, argc);
  free_strings(envp, envc);
  return PyLong_FromLong((long)pid);

fail:
  free_strings(argv, argc);
  free_strings(envp, envc);
  return NULL;
}

static PyMethodDef methods[] = {
    {"spawn", py_spawn, METH_VARARGS,
     "spawn(argv, env_items, cwd, stdin_fd, stdout_fd, stderr_fd, parent_pid) -> pid\n\n"
     "Clone(CLONE_VM|CLONE_VFORK)-spawn argv with pdeathsig, setsid, the given stdio\n"
     "fds, default signal dispositions, and close-from-3; raises the child's errno."},
    {NULL, NULL, 0, NULL}};

static struct PyModuleDef module = {PyModuleDef_HEAD_INIT, "_vfkspawn",   NULL,   -1,
                                    methods,               NULL,          NULL,   NULL,
                                    NULL};

PyMODINIT_FUNC PyInit__vfkspawn(void) { return PyModule_Create(&module); }
