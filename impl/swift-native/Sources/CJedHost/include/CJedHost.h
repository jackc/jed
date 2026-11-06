#ifndef JED_HOST_H
#define JED_HOST_H
#include <stddef.h>
#include <sys/types.h>

/* Atomic no-replace publication. Returns 0 or -1 with errno; borrows both
 * NUL-terminated paths for this call and retains no pointers. No database or
 * storage policy lives in this adapter. */
int jed_host_rename_noreplace(const char *source, const char *destination);

/* Synchronous borrowed write with SIGPIPE blocked only on this thread. */
ssize_t jed_host_write_nosigpipe(int descriptor, const void *bytes, size_t count);

/* Spawn one argv-based host transport with three close-on-exec pipes. The
 * caller owns all returned descriptors, including the stable process descriptor.
 * Returns 0 or -1 with errno; borrows argv during this call. */
int jed_host_spawn_transport(char *const argv[], int *process,
                             int *input, int *output, int *errors);

/* Signal/reap only the process identified by this stable descriptor. Waiting
 * returns 1 when exited, 0 on timeout, or -1 with errno. */
int jed_host_signal_transport(int process, int signal_number);
int jed_host_wait_transport(int process, int milliseconds);

#endif
