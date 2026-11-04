import java.io.File
import org.apache.tools.ant.taskdefs.condition.Os
import org.gradle.api.DefaultTask
import org.gradle.api.GradleException
import org.gradle.api.logging.LogLevel
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.TaskAction

open class BuildTask : DefaultTask() {
    @Input
    var rootDirRel: String? = null
    @Input
    var target: String? = null
    @Input
    var release: Boolean? = null

    @TaskAction
    fun assemble() {
        // On Windows the generated fallback list (`.exe`, `.cmd`, `.bat`) is
        // case-sensitive against the actual file extension on disk. pnpm
        // ships as `pnpm.CMD` which the default list can't resolve, so
        // Gradle's exec blows up with "command 'pnpm.bat' not found".
        // Resolve the executable up-front via `where` (Windows) / `which`
        // (Unix) so we pass an absolute path to Gradle and side-step the
        // PATH + extension dance entirely.
        val executable = resolveExecutable("pnpm") ?: "pnpm"
        try {
            runTauriCli(executable)
        } catch (e: Exception) {
            if (Os.isFamily(Os.FAMILY_WINDOWS)) {
                val fallbacks = listOf(
                    "$executable.exe",
                    "$executable.cmd",
                    "$executable.CMD",
                    "$executable.bat",
                )

                var lastException: Exception = e
                for (fallback in fallbacks) {
                    try {
                        runTauriCli(fallback)
                        return
                    } catch (fallbackException: Exception) {
                        lastException = fallbackException
                    }
                }
                throw lastException
            } else {
                throw e;
            }
        }
    }

    /**
     * Return the absolute path of `name` on PATH, or null if not found.
     * Uses `where` on Windows (which prints multiple matches, we take the
     * first), `which` elsewhere. Keeps the rest of the task agnostic about
     * exactly *how* the binary resolves — fixes the Windows pnpm.CMD case
     * sensitivity issue and also gives clearer errors if pnpm is genuinely
     * missing.
     */
    private fun resolveExecutable(name: String): String? {
        val finder = if (Os.isFamily(Os.FAMILY_WINDOWS)) "where" else "which"
        return try {
            val process = ProcessBuilder(finder, name)
                .redirectErrorStream(true)
                .start()
            val output = process.inputStream.bufferedReader().readText().trim()
            val exitCode = process.waitFor()
            if (exitCode != 0 || output.isEmpty()) {
                null
            } else {
                // `where` can print several matches, one per line; take the
                // first that actually exists on disk.
                output.lineSequence()
                    .map { it.trim() }
                    .firstOrNull { File(it).exists() }
            }
        } catch (_: Exception) {
            null
        }
    }

    fun runTauriCli(executable: String) {
        val rootDirRel = rootDirRel ?: throw GradleException("rootDirRel cannot be null")
        val target = target ?: throw GradleException("target cannot be null")
        val release = release ?: throw GradleException("release cannot be null")
        val args = listOf("tauri", "android", "android-studio-script");

        project.exec {
            workingDir(File(project.projectDir, rootDirRel))
            executable(executable)
            args(args)
            if (project.logger.isEnabled(LogLevel.DEBUG)) {
                args("-vv")
            } else if (project.logger.isEnabled(LogLevel.INFO)) {
                args("-v")
            }
            if (release) {
                args("--release")
            }
            args(listOf("--target", target))
        }.assertNormalExitValue()
    }
}