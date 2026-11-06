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
