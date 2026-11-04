/*
 * Runs one vertex and fragment shader pair through Mesa's lima driver under drm-shim, so that
 * LIMA_DEBUG=dump writes everything the driver made of it to lima.dump.0000 in the current
 * directory. tools/shaderc/shaderc.py runs it and reads both the dump and what this prints.
 *
 * usage: harness VERTEX FRAGMENT ATTRIBUTE...
 *
 * The attributes get locations 0, 1, ... in the order given, which is the vertex shader's
 * order of declaration. Every active uniform but the samplers is set to sentinel values, which
 * shaderc.py looks for in the dumped uniform buffers to learn where each lives: the k-th
 * uniform's i-th float is 64 (k + 1) + i, exact in half floats too. Each sampler gets a texture
 * unit of its own, in order. One triangle is drawn into a 64 x 64 target, no depth, no blending.
 *
 * Prints, a line each:
 *   attribute NAME COMPONENTS
 *   uniform NAME FLOATS BASE
 *   sampler NAME UNIT
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define SIZE 64
#define MAX_FLOATS 63

static char *slurp(const char *path) {
    FILE *file = fopen(path, "rb");
    if (!file) {
        perror(path);
        exit(1);
    }
    fseek(file, 0, SEEK_END);
    long length = ftell(file);
    rewind(file);
    char *text = malloc(length + 1);
    if (fread(text, 1, length, file) != (size_t)length) {
        perror(path);
        exit(1);
    }
    text[length] = 0;
    fclose(file);
    return text;
}

static GLuint shader(GLenum type, const char *path) {
    char *source = slurp(path);
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, (const char **)&source, NULL);
    glCompileShader(s);
    GLint ok;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[4096];
        glGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "%s: %s\n", path, log);
        exit(1);
    }
    free(source);
    return s;
}

/* Floats in one element of a uniform or attribute of this type; 0 for others. */
static int floats_of(GLenum type) {
    switch (type) {
    case GL_FLOAT: case GL_INT: case GL_BOOL: return 1;
    case GL_FLOAT_VEC2: case GL_INT_VEC2: case GL_BOOL_VEC2: return 2;
    case GL_FLOAT_VEC3: case GL_INT_VEC3: case GL_BOOL_VEC3: return 3;
    case GL_FLOAT_VEC4: case GL_INT_VEC4: case GL_BOOL_VEC4: case GL_FLOAT_MAT2: return 4;
    case GL_FLOAT_MAT3: return 9;
    case GL_FLOAT_MAT4: return 16;
    default: return 0;
    }
}

static int is_int(GLenum type) {
    return type == GL_INT || type == GL_INT_VEC2 || type == GL_INT_VEC3 || type == GL_INT_VEC4 || type == GL_BOOL
        || type == GL_BOOL_VEC2 || type == GL_BOOL_VEC3 || type == GL_BOOL_VEC4;
}

static void set_uniform(GLint location, GLenum type, GLint count, const GLfloat *values) {
    if (is_int(type)) {
        GLint ints[MAX_FLOATS];
        for (int i = 0; i < floats_of(type) * count; i++)
            ints[i] = (GLint)values[i];
        switch (floats_of(type)) {
        case 1: glUniform1iv(location, count, ints); break;
        case 2: glUniform2iv(location, count, ints); break;
        case 3: glUniform3iv(location, count, ints); break;
        default: glUniform4iv(location, count, ints); break;
        }
        return;
    }
    switch (type) {
    case GL_FLOAT: glUniform1fv(location, count, values); break;
    case GL_FLOAT_VEC2: glUniform2fv(location, count, values); break;
    case GL_FLOAT_VEC3: glUniform3fv(location, count, values); break;
    case GL_FLOAT_VEC4: glUniform4fv(location, count, values); break;
    case GL_FLOAT_MAT2: glUniformMatrix2fv(location, count, GL_FALSE, values); break;
    case GL_FLOAT_MAT3: glUniformMatrix3fv(location, count, GL_FALSE, values); break;
    default: glUniformMatrix4fv(location, count, GL_FALSE, values); break;
    }
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: harness VERTEX FRAGMENT ATTRIBUTE...\n");
        return 1;
    }
    PFNEGLGETPLATFORMDISPLAYEXTPROC get = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay display = get ? get(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL) : EGL_NO_DISPLAY;
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL)) {
        fprintf(stderr, "harness: no EGL display: is Mesa's vendor and the lima shim loaded?\n");
        return 1;
    }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext context = eglCreateContext(display, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, attributes);
    if (!context || !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, context)) {
        fprintf(stderr, "harness: no context\n");
        return 1;
    }
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    if (!renderer || strcmp(renderer, "Mali400") != 0) {
        fprintf(stderr, "harness: renderer %s, not Mali400\n", renderer ? renderer : "none");
        return 1;
    }

    GLuint color, framebuffer;
    glGenRenderbuffers(1, &color);
    glBindRenderbuffer(GL_RENDERBUFFER, color);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8_OES, SIZE, SIZE);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, color);

    GLuint program = glCreateProgram();
    glAttachShader(program, shader(GL_VERTEX_SHADER, argv[1]));
    glAttachShader(program, shader(GL_FRAGMENT_SHADER, argv[2]));
    int attribute_count = argc - 3;
    for (int i = 0; i < attribute_count; i++)
        glBindAttribLocation(program, i, argv[3 + i]);
    glLinkProgram(program);
    GLint ok;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[4096];
        glGetProgramInfoLog(program, sizeof log, NULL, log);
        fprintf(stderr, "link: %s\n", log);
        return 1;
    }
    glUseProgram(program);

    /* Three vertices of every attribute; the position's make a triangle in the middle. */
    static const GLfloat triangle[3][4] = {{-0.5f, -0.5f, 0.0f, 1.0f}, {0.5f, -0.5f, 0.0f, 1.0f}, {0.0f, 0.5f, 0.0f, 1.0f}};
    for (int i = 0; i < attribute_count; i++) {
        GLint active;
        glGetProgramiv(program, GL_ACTIVE_ATTRIBUTES, &active);
        int components = 0;
        for (int a = 0; a < active; a++) {
            char name[256];
            GLint size;
            GLenum type;
            glGetActiveAttrib(program, a, sizeof name, NULL, &size, &type, name);
            if (strcmp(name, argv[3 + i]) == 0)
                components = floats_of(type);
        }
        if (components == 0 || components > 4) {
            fprintf(stderr, "harness: attribute %s is not used, or not a float to vec4\n", argv[3 + i]);
            return 1;
        }
        glVertexAttribPointer(i, components, GL_FLOAT, GL_FALSE, sizeof triangle[0], triangle);
        glEnableVertexAttribArray(i);
        printf("attribute %s %d\n", argv[3 + i], components);
    }

    GLint uniforms;
    glGetProgramiv(program, GL_ACTIVE_UNIFORMS, &uniforms);
    int sentinel = 0, unit = 0;
    for (int u = 0; u < uniforms; u++) {
        char name[256];
        GLint size;
        GLenum type;
        glGetActiveUniform(program, u, sizeof name, NULL, &size, &type, name);
        char *bracket = strchr(name, '[');
        if (bracket)
            *bracket = 0;
        GLint location = glGetUniformLocation(program, name);
        if (type == GL_SAMPLER_2D) {
            if (size != 1) {
                fprintf(stderr, "harness: sampler array %s not handled\n", name);
                return 1;
            }
            GLuint texture;
            static const GLubyte texels[4 * 4 * 4];
            glActiveTexture(GL_TEXTURE0 + unit);
            glGenTextures(1, &texture);
            glBindTexture(GL_TEXTURE_2D, texture);
            glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 4, 4, 0, GL_RGBA, GL_UNSIGNED_BYTE, texels);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glUniform1i(location, unit);
            printf("sampler %s %d\n", name, unit);
            unit++;
            continue;
        }
        int floats = floats_of(type) * size;
        if (floats_of(type) == 0 || floats > MAX_FLOATS) {
            fprintf(stderr, "harness: uniform %s: type %#x x %d not handled\n", name, type, size);
            return 1;
        }
        sentinel++;
        if (64 * (sentinel + 1) > 2048) {
            fprintf(stderr, "harness: too many uniforms for the sentinels\n");
            return 1;
        }
        GLfloat values[MAX_FLOATS];
        for (int i = 0; i < floats; i++)
            values[i] = 64.0f * sentinel + i;
        set_uniform(location, type, size, values);
        printf("uniform %s %d %d\n", name, floats, 64 * sentinel);
    }

    glViewport(0, 0, SIZE, SIZE);
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    /* glFinish waits forever under the shim; a flush submits the job, and with it the dump. */
    glFlush();
    GLenum error = glGetError();
    if (error != GL_NO_ERROR) {
        fprintf(stderr, "harness: GL error %#x\n", error);
        return 1;
    }
    return 0;
}
