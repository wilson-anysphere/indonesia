public class Main {
    public static void main(String[] args) throws Exception {
        // Give the debugger a moment to install breakpoints after attach.
        Thread.sleep(500);
        int answer = 42; // BREAKPOINT_LINE
        System.out.println("answer=" + answer);
    }
}
