#ifndef JED_HOST_H
#define JED_HOST_H

/* Atomic no-replace publication. Returns 0 or -1 with errno; borrows both
 * NUL-terminated paths for this call and retains no pointers. No database or
 * storage policy lives in this adapter. */
int jed_host_rename_noreplace(const char *source, const char *destination);

#endif
