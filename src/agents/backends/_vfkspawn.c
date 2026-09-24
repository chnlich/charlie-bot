/* vfork-shaped spawn for the piped backend transports.
 *
 * Popen with a preexec_fn forces the kernel's full fork, whose page-table copy
 * scales with the forking process's resident set (~55 us/MB here — seconds at
 * the server's multi-GB RSS). The piped transports only need pdeathsig
 * child-side, so this stub spawns via clone(CLONE_VM|CLONE_VFORK) — no
 * page-table copy — and runs the fixed child sequence (pdeathsig, setsid,
 * stdio dup2, default signal dispositions, close-from-3, execve) as pure
 * syscalls: the child shares the parent's address space, so nothing here may
 * allocate or call back into Python. Setup and execve failures report the
 * child's errno over a CLOEXEC pipe and exit 127; a successful execve closes
 * that pipe, so the parent reads EOF as success.
 */
#define _GNU_SOURCE
#include <Python.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
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
  execve(a->argv[0], a->argv, a->envp);
  report_and_exit(a, errno);
  return 127; /* unreachable; keeps the compiler honest about the return type */
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
  /* PyUnicode_AsUTF8AndSize caches inside the objects; the caller's lists keep
     them alive through execve, so borrowed pointers need no extra pinning. */
  char **argv = PyMem_Malloc(((size_t)argc + 1) * sizeof(char *));
  char **envp = PyMem_Malloc(((size_t)envc + 1) * sizeof(char *));
  if (argv == NULL || envp == NULL) {
    PyMem_Free(argv);
    PyMem_Free(envp);
    return PyErr_NoMemory();
  }
  PyObject *lists[2] = {argv_obj, env_obj};
  char **arrays[2] = {argv, envp};
  Py_ssize_t sizes[2] = {argc, envc};
  for (int half = 0; half < 2; half++)
    for (Py_ssize_t i = 0; i < sizes[half]; i++) {
      PyObject *item = PyList_GetItem(lists[half], i); /* borrowed */
      arrays[half][i] = item ? (char *)PyUnicode_AsUTF8AndSize(item, NULL) : NULL;
      if (arrays[half][i] == NULL)
        goto fail;
    }
  argv[argc] = NULL;
  envp[envc] = NULL;

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
  struct spawn_args a = {argv,   envp,   cwd,  in_fd,  out_fd, err_fd, report_w,
                         (pid_t)parent_pid, {{0}}};
  sigset_t all;
  sigfillset(&all);
  /* The blocked mask is inherited so no handler runs in the child; the child
     restores the saved mask right before execve. */
  pthread_sigmask(SIG_BLOCK, &all, &a.oldmask);
  pid_t pid = clone(child_main, (char *)stack + 256 * 1024, CLONE_VM | CLONE_VFORK | SIGCHLD, &a);
  int clone_errno = errno;
  pthread_sigmask(SIG_SETMASK, &a.oldmask, NULL);
  munmap(stack, 256 * 1024);
  close(report_w);
  if (pid < 0) {
    close(report[0]);
    errno = clone_errno;
    PyErr_SetFromErrno(PyExc_OSError);
    goto fail;
  }
  /* The child has execed or exited here (CLONE_VFORK), so its argv/envp reads
     are done and the arrays can go back before the parent touches Python. */
  int err = 0;
  ssize_t got = read(report[0], &err, sizeof(err));
  close(report[0]);
  if (got == (ssize_t)sizeof(err)) {
    int status;
    while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {
    }
    errno = err;
    PyErr_SetFromErrnoWithFilename(PyExc_OSError, argv[0]);
    goto fail;
  }
  if (got == 0)
    return PyLong_FromLong((long)pid);
  PyErr_SetFromErrno(PyExc_OSError);

fail:
  PyMem_Free(argv);
  PyMem_Free(envp);
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
