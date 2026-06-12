/** Resolve the absolute path to the platform `blit` binary (throws if none installed). */
export declare function binaryPath(): string;
/** Executable filename for this platform (`blit` or `blit.exe`). */
export declare function binaryName(): string;
/** Candidate npm package names for this platform, in resolution order. */
export declare function candidatePackages(): string[];
/** Whether the current Linux runtime uses musl libc. */
export declare function isMusl(): boolean;
