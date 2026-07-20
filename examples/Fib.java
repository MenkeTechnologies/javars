// Iterative Fibonacci — compiled to fusevm bytecode, hot loop trace-JITed.
public class Fib {
    public static void main(String[] args) {
        int a = 0;
        int b = 1;
        for (int i = 0; i < 10; i++) {
            System.out.println(a);
            int next = a + b;
            a = b;
            b = next;
        }
    }
}
