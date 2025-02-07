use bastion::prelude::*;

fn main() {
    Bastion::platform();

    let message = String::from("Some message to be passed");

    Bastion::spawn(
        |context: BastionContext, msg: Box<dyn Message>| {
            // Message can be selected with receiver here.
            receive! { msg,
                String => |e| { println!("string :: {}", e)},
                i32 => |e| {println!("i32 :: {}", e)},
                _ => println!("No message as expected. Default")
            }

            // Do some other job in process body
            println!("root supervisor - spawn_at_root - 1");

            // Rebind to the system
            context.hook();
        },
        message,
    );

    Bastion::start()
}
