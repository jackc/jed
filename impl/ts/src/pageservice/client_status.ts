// The page client's connection status buffer (spec/pageservice/README.md §6.5), shared by the engine
// thread (client.ts) and the socket worker (client_worker.ts), which writes it. Its own module so the
// engine thread never loads the worker's module.
//
// The status buffer's layout (STATUS_BYTES bytes): Int32 slot STATUS_LOST is 1 once the connection
// ended; STATUS_PAUSED is 1 after a yield-begun notice (until a yield-cancelled one), whose
// connection id is the BigInt64 at byte NOTICE_CONN_OFFSET; the Float64 at LOST_AT_OFFSET is when the
// connection ended (Date.now()).
export const STATUS_BYTES = 24;
export const STATUS_LOST = 0;
export const STATUS_PAUSED = 1;
export const NOTICE_CONN_OFFSET = 8;
export const LOST_AT_OFFSET = 16;
