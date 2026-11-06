#ifndef _GNU_SOURCE
#define _GNU_SOURCE 1
#endif
#include "CJedHost.h"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>

int jed_host_rename_noreplace(const char *source, const char *destination) {
#if defined(__linux__)
    return renameat2(AT_FDCWD, source, AT_FDCWD, destination, RENAME_NOREPLACE);
#elif defined(__APPLE__)
    return renameatx_np(AT_FDCWD, source, AT_FDCWD, destination, RENAME_EXCL);
#else
    (void)source;
    (void)destination;
    errno = ENOSYS;
    return -1;
#endif
}

#include <pthread.h>
#include <signal.h>
#include <unistd.h>
#include <time.h>

ssize_t jed_host_write_nosigpipe(int descriptor, const void *bytes, size_t count) {
    sigset_t blocked, previous, pending;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGPIPE);
    int error = pthread_sigmask(SIG_BLOCK, &blocked, &previous);
    if (error != 0) { errno = error; return -1; }
    sigpending(&pending);
    int already_pending = sigismember(&pending, SIGPIPE);
    ssize_t result = write(descriptor, bytes, count);
    int saved = errno;
    if (result < 0 && saved == EPIPE && !already_pending) {
#if defined(__linux__)
        struct timespec zero = {0, 0};
        while (sigtimedwait(&blocked, NULL, &zero) < 0 && errno == EINTR) {}
#else
        if (sigpending(&pending) == 0 && sigismember(&pending, SIGPIPE)) {
            int signal_number;
            sigwait(&blocked, &signal_number);
        }
#endif
    }
    pthread_sigmask(SIG_SETMASK, &previous, NULL);
    errno = saved;
    return result;
}

#include <spawn.h>
#if defined(__linux__) && defined(__GLIBC__)
#if __GLIBC_PREREQ(2, 39)
#include <sys/pidfd.h>
#include <sys/wait.h>
#include <poll.h>
#define JED_HAVE_PIDFD_SPAWN 1
#endif
#endif
extern char **environ;

/* Keep all pipe endpoints above standard descriptors. This also handles an
 * embedding host that deliberately closed stdin, stdout, or stderr. */
static int transport_pipe(int descriptors[2]) {
#if defined(__linux__)
    if (pipe2(descriptors, O_CLOEXEC) < 0) return -1;
#else
    if (pipe(descriptors) < 0) return -1;
    if (fcntl(descriptors[0], F_SETFD, FD_CLOEXEC) < 0 ||
        fcntl(descriptors[1], F_SETFD, FD_CLOEXEC) < 0) {
        int saved = errno;
        close(descriptors[0]); close(descriptors[1]); errno = saved;
        return -1;
    }
#endif
    for (int i = 0; i < 2; ++i) {
        if (descriptors[i] >= 3) continue;
        int replacement = fcntl(descriptors[i], F_DUPFD_CLOEXEC, 3);
        if (replacement < 0) {
            int saved = errno;
            close(descriptors[0]); close(descriptors[1]); errno = saved;
            return -1;
        }
        close(descriptors[i]); descriptors[i] = replacement;
    }
    return 0;
}

int jed_host_spawn_transport(char *const argv[], int *process,
                             int *input, int *output, int *errors) {
#if !defined(JED_HAVE_PIDFD_SPAWN)
    (void)argv;
    *process = -1; *input = -1; *output = -1; *errors = -1;
    errno = ENOSYS;
    return -1;
#else
    int pipes[3][2] = {{-1, -1}, {-1, -1}, {-1, -1}};
    int error = 0, initialized = 0;
    posix_spawn_file_actions_t actions;
    *process = -1; *input = -1; *output = -1; *errors = -1;
    for (int i = 0; i < 3; ++i) {
        if (transport_pipe(pipes[i]) < 0) {
            /* transport_pipe has already closed both endpoints on failure. */
            pipes[i][0] = -1; pipes[i][1] = -1; error = errno; goto failed;
        }
    }
    error = posix_spawn_file_actions_init(&actions);
    if (error != 0) goto failed;
    initialized = 1;
    if ((error = posix_spawn_file_actions_adddup2(&actions, pipes[0][0], STDIN_FILENO)) != 0 ||
        (error = posix_spawn_file_actions_adddup2(&actions, pipes[1][1], STDOUT_FILENO)) != 0 ||
        (error = posix_spawn_file_actions_adddup2(&actions, pipes[2][1], STDERR_FILENO)) != 0) goto failed;
    for (int i = 0; i < 3; ++i) {
        for (int j = 0; j < 2; ++j) {
            error = posix_spawn_file_actions_addclose(&actions, pipes[i][j]);
            if (error != 0) goto failed;
        }
    }
    error = pidfd_spawnp(process, argv[0], &actions, NULL, argv, environ);
    if (error != 0) goto failed;
    posix_spawn_file_actions_destroy(&actions);
    close(pipes[0][0]); close(pipes[1][1]); close(pipes[2][1]);
    *input = pipes[0][1]; *output = pipes[1][0]; *errors = pipes[2][0];
    return 0;
failed:
    if (initialized) posix_spawn_file_actions_destroy(&actions);
    for (int i = 0; i < 3; ++i)
        for (int j = 0; j < 2; ++j)
            if (pipes[i][j] >= 0) close(pipes[i][j]);
    errno = error;
    return -1;
#endif
}

int jed_host_signal_transport(int process, int signal_number) {
#if defined(JED_HAVE_PIDFD_SPAWN)
    return pidfd_send_signal(process, signal_number, NULL, 0);
#else
    (void)process; (void)signal_number; errno = ENOSYS; return -1;
#endif
}

int jed_host_wait_transport(int process, int milliseconds) {
#if defined(JED_HAVE_PIDFD_SPAWN)
    struct pollfd item = {process, POLLIN, 0};
    int ready;
    do { ready = poll(&item, 1, milliseconds); } while (ready < 0 && errno == EINTR);
    if (ready <= 0) return ready;
    if ((item.revents & POLLNVAL) != 0) { errno = EBADF; return -1; }
    if ((item.revents & (POLLIN | POLLHUP)) == 0) { errno = EIO; return -1; }
    /* An embedding host may already have reaped the child or requested
     * automatic SIGCHLD reaping. The pidfd still proves that it has exited. */
    siginfo_t info;
    while (waitid(P_PIDFD, (id_t)process, &info, WEXITED | WNOHANG) < 0 && errno == EINTR) {}
    return 1;
#else
    (void)process; (void)milliseconds; errno = ENOSYS; return -1;
#endif
}
