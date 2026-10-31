// The npm packages the release is published as, which build.ts creates and
// install_api.ts looks the binary up in.

/** The scope of the packages. */
export const scope = "@kprint";

/** The package users install, with the `dprint` and `kprint` commands. */
export const mainPackage = `${scope}/kprint`;

/** The prefix of the platform packages, also of their tarball file names. */
const platformPrefix = "kprint";

/**
 * The package with the binary for a platform (ex. `@kprint/kprint-win32-x64`
 * for `win32-x64`), which is also its directory in the scope's.
 */
export function platformPackage(platform: string): string {
  return `${scope}/${platformDir(platform)}`;
}

/** The directory, and tarball file name, of a platform's package. */
export function platformDir(platform: string): string {
  return `${platformPrefix}-${platform}`;
}
