import java.io.ByteArrayInputStream;
import java.io.InputStream;

class TryWithResources {
  int readFirstByte() throws Exception {
    try (InputStream in = new ByteArrayInputStream("x".getBytes())) {
      return in.read();
    }
  }
}

