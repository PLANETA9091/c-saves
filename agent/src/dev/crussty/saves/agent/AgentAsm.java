package dev.crussty.saves.agent;

import org.objectweb.asm.ClassReader;
import org.objectweb.asm.ClassVisitor;
import org.objectweb.asm.ClassWriter;
import org.objectweb.asm.Handle;
import org.objectweb.asm.Label;
import org.objectweb.asm.MethodVisitor;
import org.objectweb.asm.Opcodes;

/**
 * ASM replace_body for the standalone agent (v0.9.62).
 *
 * Same rewrite the CRUSSTY kernel's SdkAsmHelper performs (cplug-sdk), carried
 * in-tree so a plain -javaagent can do it without the kernel: a method body is
 * replaced with an invokestatic of a bridge followed by a return. COMPUTE_FRAMES
 * re-frames the output (Paper 26.3 classes are major 69 / Java 25 — ASM 9.11.1
 * is required; 9.7.1 rejects major 69).
 *
 * The bridge descriptor's return type must equal the patched method's.
 * Arg list: (slot, typeChar) pairs; slot 0 = this for instance methods.
 */
public final class AgentAsm {

    private static volatile String lastError = "";

    private AgentAsm() {}

    public static String lastError() { return lastError; }

    /**
     * @param args per-arg [slot, typeChar] pairs; typeChar one of I J D F Z C S B L [
     * @return rewritten class bytes, or null on failure (lastError() carries the reason)
     */
    public static byte[] replaceBody(byte[] cls, String methodName, String methodDesc,
            String bridgeOwner, String bridgeName, String bridgeDesc, int[][] args) {
        lastError = "";
        try {
            boolean[] found = new boolean[1];
            ClassReader cr = new ClassReader(cls);
            ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override
                public MethodVisitor visitMethod(int access, String name, String desc,
                        String signature, String[] exceptions) {
                    MethodVisitor mv = super.visitMethod(access, name, desc, signature, exceptions);
                    if (!name.equals(methodName) || !desc.equals(methodDesc)) {
                        return mv;
                    }
                    found[0] = true;
                    // Consuming wrapper: emit the new body in visitCode, swallow
                    // every other instruction event so the original body never
                    // reaches the writer. visitEnd still forwards to close the
                    // method's attribute list.
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override
                        public void visitCode() {
                            mv.visitCode();
                            for (int[] arg : args) {
                                mv.visitVarInsn(loadOpcode((char) arg[1]), arg[0]);
                            }
                            mv.visitMethodInsn(Opcodes.INVOKESTATIC,
                                    bridgeOwner, bridgeName, bridgeDesc, false);
                            mv.visitInsn(returnOpcode(methodDesc.charAt(methodDesc.length() - 1)));
                            // COMPUTE_FRAMES also recomputes maxs; values ignored.
                            mv.visitMaxs(0, 0);
                        }

                        @Override public void visitInsn(int i) { }
                        @Override public void visitIntInsn(int i, int v) { }
                        @Override public void visitVarInsn(int i, int v) { }
                        @Override public void visitTypeInsn(int i, String s) { }
                        @Override public void visitFieldInsn(int i, String a, String b, String c) { }
                        @Override public void visitMethodInsn(int i, String a, String b, String c, boolean d) { }
                        @Override public void visitInvokeDynamicInsn(String a, String b, Handle h, Object... o) { }
                        @Override public void visitJumpInsn(int i, Label l) { }
                        @Override public void visitLabel(Label l) { }
                        @Override public void visitLdcInsn(Object o) { }
                        @Override public void visitIincInsn(int i, int v) { }
                        @Override public void visitTableSwitchInsn(int a, int b, Label l, Label... ls) { }
                        @Override public void visitLookupSwitchInsn(Label l, int[] k, Label[] ls) { }
                        @Override public void visitMultiANewArrayInsn(String s, int d) { }
                        @Override public void visitTryCatchBlock(Label a, Label b, Label c, String s) { }
                        @Override public void visitLineNumber(int i, Label l) { }
                        @Override public void visitLocalVariable(String a, String b, String c, Label l, Label m, int i) { }
                        @Override public void visitFrame(int a, int b, Object[] c, int d, Object[] e) { }
                        @Override public void visitMaxs(int a, int b) { }
                    };
                }
            }, 0);
            if (!found[0]) {
                lastError = "method " + methodName + methodDesc + " not found in class";
                return null;
            }
            return cw.toByteArray();
        } catch (Throwable t) {
            lastError = t.toString();
            return null;
        }
    }

    private static int loadOpcode(char t) {
        switch (t) {
            case 'J': return Opcodes.LLOAD;
            case 'D': return Opcodes.DLOAD;
            case 'F': return Opcodes.FLOAD;
            case 'I':
            case 'Z':
            case 'B':
            case 'C':
            case 'S': return Opcodes.ILOAD;
            default: return Opcodes.ALOAD; // L... or [
        }
    }

    private static int returnOpcode(char t) {
        switch (t) {
            case 'V': return Opcodes.RETURN;
            case 'J': return Opcodes.LRETURN;
            case 'D': return Opcodes.DRETURN;
            case 'F': return Opcodes.FRETURN;
            case 'I':
            case 'Z':
            case 'B':
            case 'C':
            case 'S': return Opcodes.IRETURN;
            default: return Opcodes.ARETURN;
        }
    }
}
